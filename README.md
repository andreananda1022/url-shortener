# URL Shortener & Real-Time Analytics Engine

A high-performance URL shortener built with **Rust**, **Axum**, **PostgreSQL** and **Redis**. It features JWT authentication, a cache-aside redirect path, per-user rate limiting, and an asynchronous click-analytics pipeline that batches events through Redis before persisting them to PostgreSQL.

> Built as a learning-driven portfolio project, with a focus on the hard parts of backend engineering: **cache invalidation**, **race conditions**, and **write batching**.

---

## Highlights

- **Fast redirects**: cache-aside pattern with Redis in front of PostgreSQL; cache misses are back-filled off the request path.
- **Correct cache invalidation**: updating a link deletes its cache entry (*invalidate-on-write*), so users never get redirected to a stale URL.
- **JWT authentication**: register/login with bcrypt-hashed passwords, stateless bearer tokens, and a reusable Axum extractor for protected routes.
- **Rate limiting**: per-user limit (10 requests / 60 s) implemented with Redis atomic `INCR` + `EXPIRE`.
- **Asynchronous analytics**: every click is pushed to a Redis list (fire-and-forget), then a background worker flushes events to PostgreSQL in a **single batch `INSERT ... UNNEST`**.
- **Collision-safe short codes**: random 7-char [nanoid](https://crates.io/crates/nanoid) backed by a database `UNIQUE` constraint, with bounded retry on collision.
- **Compile-time checked SQL** via `sqlx::query!`.

---

## Architecture

```mermaid
flowchart LR
    C[Client] -->|HTTP| A[Axum API]

    subgraph Request path
      A -->|1. GET short_code| R[(Redis)]
      R -. cache miss .-> P[(PostgreSQL)]
      P -. backfill via tokio::spawn .-> R
    end

    A -->|RPUSH click event| Q[(Redis list: click_events)]
    W[Background worker<br/>every 10s] -->|LRANGE| Q
    W -->|batch INSERT UNNEST| P
    W -->|LTRIM processed items| Q

    A -->|INCR rate_limit:user| R
```

**Source of truth:** PostgreSQL. **Redis** is used as a cache, a rate-limit counter store, and a short-lived event buffer. Nothing in Redis is required to be durable.

### Redirect flow (cache-aside)

1. Record a click event (async, non-blocking).
2. `GET short_code` from Redis.
3. **Hit** → respond `302 Found` immediately.
4. **Miss** → query PostgreSQL → back-fill Redis in a background task → respond `302`.
5. Not found anywhere → `404`.

### Analytics flow (batching)

1. Each redirect serializes a `ClickEvent` (short code, user-agent, IP, timestamp) and `RPUSH`es it to `click_events`.
2. Every 10 seconds the worker reads the whole list with `LRANGE`.
3. Events are transposed into column arrays and written with **one** `INSERT ... SELECT * FROM UNNEST(...)`.
4. Only after the insert succeeds does the worker `LTRIM` exactly the items it processed. Clicks that arrived mid-flush are kept for the next round.

---

## Tech Stack

| Concern | Choice |
|---|---|
| Language / runtime | Rust (edition 2024), Tokio |
| Web framework | Axum 0.8 |
| Database | PostgreSQL 16 via SQLx 0.9 (with migrations) |
| Cache / counters / queue | Redis 7 (`redis` crate, `ConnectionManager`) |
| Auth | `jsonwebtoken` (HS256), `bcrypt` |
| Serialization | `serde`, `serde_json` |
| Misc | `nanoid`, `chrono`, `uuid`, `dotenvy` |
| Infra | Docker Compose |

---

## Getting Started

### Prerequisites

- Rust (stable, recent enough for edition 2024)
- Docker & Docker Compose
- [`sqlx-cli`](https://crates.io/crates/sqlx-cli): `cargo install sqlx-cli --no-default-features --features rustls,postgres`

### 1. Clone and configure

```bash
git clone <your-repo-url>
cd url-shortener
```

Create a `.env` file in the project root:

```dotenv
DATABASE_URL=postgres://admin:admin123@127.0.0.1:5432/url_shortener
REDIS_URL=redis://127.0.0.1:6379/
JWT_SECRET=replace-with-a-long-random-string
```

> The credentials above match `docker-compose.yml` and are for **local development only**. Use a long, random `JWT_SECRET` and real credentials anywhere else.

### 2. Start PostgreSQL and Redis

```bash
docker compose up -d
```

Both containers are bound to `127.0.0.1` only, so they are not exposed to your network.

### 3. Run migrations

```bash
sqlx migrate run
```

### 4. Run the server

```bash
cargo run
```

The API listens on `http://127.0.0.1:8080`.

> **Note:** because SQLx validates queries at compile time, PostgreSQL must be running (with migrations applied) whenever you run `cargo build`.

---

## API Reference

| Method | Path | Auth | Description |
|---|---|---|---|
| `POST` | `/register` | – | Create a user (`201`, or `409` if username is taken) |
| `POST` | `/login` | – | Verify credentials, returns a JWT (`401` on any failure) |
| `POST` | `/shorten` | Bearer JWT + rate limit | Create a short URL |
| `GET` | `/{short_code}` | – | Redirect (`302`) to the original URL |
| `PATCH` | `/{short_code}` | – | Update the destination URL and invalidate its cache entry |
| `GET` | `/health` | – | Checks PostgreSQL connectivity |

### Example session

```bash
# 1. Register
curl -X POST http://127.0.0.1:8080/register \
  -H "Content-Type: application/json" \
  -d '{"username": "budi", "password": "rahasia123"}'

# 2. Login -> {"token": "<jwt>"}
curl -X POST http://127.0.0.1:8080/login \
  -H "Content-Type: application/json" \
  -d '{"username": "budi", "password": "rahasia123"}'

# 3. Create a short URL -> {"short_code": "kM6cQKu"}
curl -X POST http://127.0.0.1:8080/shorten \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <jwt>" \
  -d '{"original_url": "https://www.rust-lang.org"}'

# 4. Follow it (use --no-location to inspect the 302 itself)
curl -v --no-location http://127.0.0.1:8080/kM6cQKu

# 5. Change the destination (cache entry is invalidated)
curl -X PATCH http://127.0.0.1:8080/kM6cQKu \
  -H "Content-Type: application/json" \
  -d '{"original_url": "https://www.example.com"}'
```

The 11th `POST /shorten` within 60 seconds from the same user returns `429 Too Many Requests`.

---

## Database Schema

```
users   (id UUID PK, username UNIQUE, password_hash, created_at)
urls    (id UUID PK, short_code UNIQUE, original_url, user_id -> users.id ON DELETE CASCADE, created_at)
clicks  (id UUID PK, short_code, user_agent, ip_address, clicked_at)
```

---

## Design Decisions

| Decision | Rationale |
|---|---|
| **UUID primary keys** | Non-sequential IDs avoid enumeration (IDOR) and are safe to expose. |
| **`UNIQUE` enforced in the database** | Application-level "check then insert" is a TOCTOU race; the constraint is atomic. |
| **Retry loop for `short_code` collisions** | Collisions are an internal detail, not a client error. Retries are bounded (5) and end in a `500`, never an infinite loop. |
| **`302` instead of `301`** | Browsers must not cache the redirect permanently, so every click reaches the server and can be counted. |
| **`SET` back-fill is fire-and-forget, `DEL` is awaited** | A failed back-fill only costs one extra DB read (self-healing). A failed invalidation would serve wrong data after a `200 OK`. |
| **Generic `401` on login failures** | Same response for unknown user and wrong password prevents user enumeration. |
| **`INCR` for rate limiting** | `GET` + `SET` from application code races; `INCR` is atomic in Redis. Counters are per user, not global. |
| **Batch `INSERT ... UNNEST`** | Amortizes per-transaction overhead (WAL fsync, round trips) and protects the small connection pool from redirect traffic spikes. |
| **Insert first, then `LTRIM` N items** | At-least-once delivery: worst case is a duplicate, never a lost event. `LTRIM` (not `DEL`) keeps clicks that arrive during the flush. |
| **No foreign key from `clicks` to `urls`** | Event history should outlive the link it describes. |

---

## Known Limitations & Roadmap

Being upfront about what this project does *not* do yet:

- [ ] **`PATCH /{short_code}` is not authenticated** and does not check link ownership. It should require a JWT and verify `urls.user_id`.
- [ ] Rate limiting is a **fixed-window counter**, a simplification of a true token bucket. `INCR` and `EXPIRE` are two separate calls; a Lua script (or `SET NX EX`) would make the first-hit TTL fully atomic.
- [ ] **Geolocation** is not resolved yet; only the raw client address (including port) is stored. Next step: strip the port, honor `X-Forwarded-For` behind a proxy, and add a GeoIP lookup.
- [ ] Clicks are recorded even for short codes that do not exist.
- [ ] Cached URLs have **no TTL** (they are only invalidated on update).
- [ ] The worker reads the entire list at once; it should process bounded chunks.
- [ ] Widespread `.unwrap()` in handlers; replace with a custom error type implementing `IntoResponse`.
- [ ] Input validation (URL format, username/password rules) and an endpoint to read analytics.
- [ ] Refactor `main.rs` into modules (`handlers`, `auth`, `worker`, `models`), add integration tests, and commit SQLx offline data (`cargo sqlx prepare`) so CI can build without a database.

---

## Project Layout

```
.
├── docker-compose.yml     # PostgreSQL + Redis
├── migrations/            # SQLx migrations (users, urls, clicks)
├── src/main.rs            # Server, handlers, extractors, background worker
├── Cargo.toml
└── .env                   # local config (not committed)
```

---

## License

MIT (or your license of choice)

## Author

Your Name · [GitHub](https://github.com/your-username) · [LinkedIn](https://www.linkedin.com/in/your-profile)
