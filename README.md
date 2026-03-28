# filest

Simple REST file server with HTTP/1.1, HTTP/2, and HTTP/3 (QUIC) support.

## Configuration

All settings via environment variables (or `.env` file):

| Variable | Description | Default |
|---|---|---|
| `LISTEN_ADDR` | Bind address (TCP + UDP) | `0.0.0.0:8090` |
| `NS_<NAME>` | Register namespace `<name>` mapped to a directory on disk | — (at least one required) |
| `CERT_PATH` | Path to TLS certificate (PEM) | — |
| `KEY_PATH` | Path to TLS private key (PEM) | — |

Without `CERT_PATH`/`KEY_PATH`: TCP serves plain HTTP, QUIC uses auto-generated self-signed certs.

## API

All paths follow the pattern `/{namespace}/{path}`, where `namespace` matches one of the configured `NS_*` variables (lowercased).

Path traversal (`..`) is rejected with `400 Bad Request`.

### GET `/{namespace}/{path}`

Serve a file or list a directory.

**File response:**

```
200 OK
Content-Type: <guessed from extension>

<file bytes>
```

**Directory response:**

```
200 OK
Content-Type: application/json

{"files": ["namespace/file1.txt", "namespace/file2.txt"]}
```

| Status | Meaning |
|---|---|
| `200` | Success |
| `404` | Namespace or file not found |
| `500` | I/O error |

### PUT `/{namespace}/{path}`

Upload or overwrite a file. Parent directories are created automatically.

Request body: raw file bytes.

| Status | Meaning |
|---|---|
| `201` | File created/overwritten |
| `400` | Path traversal |
| `404` | Namespace not found |
| `500` | I/O error |

### DELETE `/{namespace}/{path}`

Remove a file.

| Status | Meaning |
|---|---|
| `204` | Deleted |
| `404` | Namespace or file not found |
| `500` | I/O error |

### PATCH `/{namespace}/{path}`

Rename/move within the same namespace.

Request body:

```json
{"destination": "new/relative/path"}
```

`destination` is relative to the namespace root. Path traversal in destination is rejected.

| Status | Meaning |
|---|---|
| `204` | Renamed |
| `400` | Path traversal in destination |
| `404` | Namespace not found or source doesn't exist |
| `409` | Destination already exists |
| `500` | I/O error |

## Build & Run

```bash
cargo build
cargo run    # requires NS_* env vars
```

Example:

```bash
NS_UPLOADS=/var/data/uploads cargo run
curl localhost:8090/uploads/hello.txt        # GET
curl -X PUT -d 'hi' localhost:8090/uploads/hello.txt  # PUT
```
