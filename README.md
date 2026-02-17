# Clawdorio

Tauri desktop shell + headless Rust engine/API (SQLite-backed).

## Headless hosting (Docker)

Pull prebuilt image (GitHub Container Registry):

```bash
docker pull ghcr.io/donsqualo/clawdorio:latest
```

Build:

```bash
docker build -t clawdorio .
```

Run (persists SQLite DB under local `./data`):

```bash
mkdir -p data
docker run --rm -p 39333:39333 -v "$PWD/data:/home/clawdorio/data" clawdorio
```

Health check:

```bash
curl http://127.0.0.1:39333/health
```

## Headless hosting (native)

```bash
cargo run -p clawdorio-server -- --host 0.0.0.0 --port 39333
```

## Desktop dev (Tauri)

```bash
npm install
npm run tauri dev
```
