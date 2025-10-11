# rust_domain_searcher_api

A minimal high-performance HTTP API in Rust that generates domain candidates and checks their HTTP reachability, exposing endpoints for live stats, per-TLD domain lists, and configured TLDs.

## Endpoints

Base URL: http://localhost:8080 (override with `-addr`, see [src/main.rs](src/main.rs))

- GET `/stats/`
  - Returns JSON with runtime/progress metrics.
  - Response fields:
    - elapsed: string
    - eta: string
    - found: integer
    - remaining: integer
    - speed_per_sec: number
    - efficiency_percent: number
    - percent: number
    - generated: integer
    - checked: integer
    - total_planned: integer
    - domains_memory_bytes: integer
    - domains_memory_human: string
    - go_mem_alloc_bytes: integer (always 0 in Rust)
  - Example:
  ```bash
  curl -s http://localhost:8080/stats/ | jq .
  ```

- GET `/domain/{tld}.txt`
- GET `/domain/{tld}.json`
- GET `/domain/__all__.txt`
- GET `/domain/__all__.json`

  - Returns discovered domain names for a specific TLD (e.g., ru, com) or all TLDs combined.
  - .txt returns newline-delimited text; .json returns a JSON array.
  - Examples:
  ```bash
  # All TLDs as text
  curl -s http://localhost:8080/domain/__all__.txt

  # Only .ru as JSON
  curl -s http://localhost:8080/domain/ru.json | jq .
  ```

- GET `/tlds/`
  - Returns JSON array of configured TLDs (without leading dot), normalized to lowercase and without duplicates.
  - Example:
  ```bash
  curl -s http://localhost:8080/tlds/ | jq .
  ```

## Configuration

The service reads YAML configuration with `-config` flag (default suggested path for systemd: `/etc/rust_domain_searcher_api/domain_search.config.yaml`). See [domain_search.config.yaml](domain_search.config.yaml) for a ready-to-use example.

Main sections and keys:

- generator:
  - tlds: explicit list of TLDs (e.g., [".ru", ".com"]); ignored if `tlds_file` is set
  - tlds_file: path or URL to a source with TLDs (e.g., IANA list)
  - min_length, max_length: label length to generate
  - alphabet: characters used to build labels
  - allow_hyphen: allow hyphen at all
  - forbid_leading_hyphen, forbid_trailing_hyphen, forbid_double_hyphen: additional hyphen rules
- limits:
  - concurrency: number of concurrent HTTP checks
  - rate_per_second: global RPS limiter
  - max_candidates: generation cap per pass
- http_check:
  - timeout: request timeout duration (e.g., "3s")
  - retry: number of retry attempts
  - method: HTTP method (e.g., "GET")
  - body_limit: max bytes to read from response body (e.g., "32KB")
  - accept_status_min, accept_status_max: HTTP status code range considered "reachable"
  - try_https_first: whether to try HTTPS before HTTP
- run:
  - loop: if true, restarts generation loop after reaching `max_candidates`
- storage:
  - dir: directory to store per-TLD text files (e.g., `/var/lib/rust_domain_searcher_api/domains`)
  - resume: enable resume from last saved position on restart
  - state_file: optional explicit path to state file (defaults to `<dir>/state.json`)

Example:

```yaml
version: 1

generator:
  # Full TLD list (IANA). If provided, overrides 'tlds' list.
  # Official, frequently updated source:
  # https://data.iana.org/TLD/tlds-alpha-by-domain.txt
  tlds_file: "https://data.iana.org/TLD/tlds-alpha-by-domain.txt"
  min_length: 1
  max_length: 20
  alphabet: "abcdefghijklmnopqrstuvwxyz0123456789-"
  allow_hyphen: true
  forbid_leading_hyphen: true
  forbid_trailing_hyphen: true
  forbid_double_hyphen: true

limits:
  concurrency: 30              # number of concurrent checks
  rate_per_second: 300         # global RPS limit
  max_candidates: 1000000000   # maximum generated domain names per pass

http_check:
  timeout: "3s"
  retry: 0
  method: "GET"
  body_limit: "32KB"
  accept_status_min: 200
  accept_status_max: 1000
  try_https_first: true

run:
  loop: false        # repeat the generation loop when max_candidates is reached

storage:
  dir: "/var/lib/rust_domain_searcher_api/domains"
  resume: true
  # state_file: "/var/lib/rust_domain_searcher_api/state.json"
```

## Run

- Using Cargo:
  ```bash
  cd rust_domain_searcher_api
  cargo run -- -addr :8080 -config ../domain_search.config.yaml
  ```

- Using Makefile:
  ```bash
  make -C rust_domain_searcher_api build
  ./rust_domain_searcher_api/bin/rust_domain_searcher_api -addr :8080 -config ../domain_search.config.yaml
  ```

- Reset storage and state:
  ```bash
  make -C rust_domain_searcher_api reset CONFIG=../domain_search.config.yaml
  ```

## Deploy (systemd)

- Install:
  ```bash
  make -C rust_domain_searcher_api install
  ```
- Service helpers:
  ```bash
  make -C rust_domain_searcher_api service-status
  make -C rust_domain_searcher_api service-restart
  make -C rust_domain_searcher_api logs
  ```

Files/paths used by deploy:
- Unit: `/etc/systemd/system/rust_domain_searcher_api.service`
- Env: `/etc/rust_domain_searcher_api/rust_domain_searcher_api.env`
- Config: `/etc/rust_domain_searcher_api/domain_search.config.yaml`
- Data dir: `/var/lib/rust_domain_searcher_api`

## License

MIT

![](https://asdertasd.site/counter/rust_domain_searcher_api)