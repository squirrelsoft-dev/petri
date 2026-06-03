# Vsock Dispatch Protocol

Petri hosts dispatch tool work to the guest agent over vsock using newline-delimited JSON (NDJSON). The protocol is request/response oriented: the host sends one complete JSON object per line, and the guest returns one complete JSON object per line with the same request id.

This document defines protocol version `1`.

## Transport And Framing

- The host opens a vsock stream to the guest agent's dispatch port.
- Every frame is a single UTF-8 JSON object followed by `\n`.
- Frames must not contain embedded newlines outside JSON string escaping.
- Empty lines are invalid.
- A frame that is not valid UTF-8, not valid JSON, or not a JSON object is malformed.
- The guest may process requests concurrently, so result frames may be returned out of order.
- The `id` field is the correlation key. The host must treat duplicate in-flight ids on one connection as invalid client behavior.

The guest should keep the stream open after request-level failures and policy rejections. It may close the stream after malformed input when it cannot recover framing safely.

## Compatibility

Every request includes a `protocol_version` integer. Version `1` is the only version defined by this spec.

The guest must reject unsupported versions with `status = "rejected"` and `error.code = "unsupported_protocol_version"` when it can parse enough of the frame to identify the request id. If no valid id is available, it returns a malformed-input error without an id.

Version `1` implementations must ignore unknown fields inside recognized objects unless this document says the object is closed. The top-level request object is open for future optional fields. The `[policy]` boot config remains closed as defined in [Immutable Policy Config](policy-config.md).

Future incompatible changes must increment `protocol_version`. Compatible additions may add optional request fields, result fields, tool-specific args, or error metadata.

## Request Schema

```json
{
  "protocol_version": 1,
  "id": "abc123",
  "tool": "bash_command",
  "args": {
    "command": "cargo",
    "argv": ["test"],
    "cwd": "/workspace"
  },
  "limits": {
    "timeout_ms": 30000,
    "max_output_bytes": 1048576
  }
}
```

| Field | Type | Required | Description |
|---|---:|---:|---|
| `protocol_version` | integer | yes | Protocol version requested by the host. Must be `1` for this spec. |
| `id` | string | yes | Host-generated request id. Must be non-empty and unique among in-flight requests on the connection. |
| `tool` | string | yes | Tool name understood by the guest agent. |
| `args` | object | yes | Tool-specific arguments. |
| `limits` | object | no | Request-scoped limits that may narrow the boot policy. |

### Limit Object

```json
{
  "timeout_ms": 30000,
  "max_output_bytes": 1048576
}
```

| Field | Type | Required | Description |
|---|---:|---:|---|
| `timeout_ms` | positive integer | no | Maximum wall-clock runtime for this request. Must not exceed the boot policy runtime cap. |
| `max_output_bytes` | positive integer | no | Maximum combined stdout and stderr bytes returned for this request. Must not exceed the boot policy output cap. |

If `limits` is omitted, the guest uses the boot policy caps. If a request limit is higher than the boot policy, the guest rejects the request.

## Bash Command Args

The initial tool name is `bash_command`, but the command is not shell text. It describes one executable launch.

```json
{
  "command": "cargo",
  "argv": ["test", "--all"],
  "cwd": "/workspace"
}
```

| Field | Type | Required | Description |
|---|---:|---:|---|
| `command` | string | yes | Executable name. Must match the immutable policy allowlist. |
| `argv` | array of strings | no | Arguments passed to the executable. Defaults to an empty array. |
| `cwd` | absolute string path | yes | Working directory. Must canonicalize inside the policy workspace root. |

The guest must not pass `command` through a shell. Shell snippets, command chaining, path traversal, and symlink escapes are rejected by policy checks.

## Result Schema

Every accepted request returns exactly one terminal result frame.

```json
{
  "protocol_version": 1,
  "id": "abc123",
  "status": "success",
  "stdout": "running tests\n",
  "stderr": "",
  "exit_code": 0,
  "elapsed_ms": 4821,
  "output_truncated": false
}
```

| Field | Type | Required | Description |
|---|---:|---:|---|
| `protocol_version` | integer | yes | Protocol version used for the result. |
| `id` | string or null | yes | Request id, or `null` when malformed input did not contain a usable id. |
| `status` | string | yes | One of `success`, `failure`, `rejected`, `timeout`, `cancelled`, or `malformed`. |
| `stdout` | string | for process results | Captured stdout, possibly truncated. |
| `stderr` | string | for process results | Captured stderr, possibly truncated. |
| `exit_code` | integer or null | for process results | Process exit code. `null` when no process exit code exists. |
| `elapsed_ms` | integer | yes | Wall-clock time spent handling the request. |
| `output_truncated` | boolean | for process results | Whether output was truncated to fit the effective output cap. |
| `error` | object | for rejected, timeout, cancelled, malformed, and guest failures | Machine-readable error details. |

`success` means the tool completed and returned exit code `0`. `failure` means the tool ran and returned a non-zero exit code, or the guest failed while preparing or running an otherwise valid request. Process failures do not need an `error` object when stdout, stderr, and exit code fully describe the result. `rejected` means the guest refused to run the request because of policy, unsupported version, unknown tool, or invalid request shape. `timeout` means the effective timeout expired. `cancelled` means the request was cancelled before completion. `malformed` means the input frame could not be interpreted as a valid request.

## Error Shape

```json
{
  "code": "policy_denied",
  "message": "command is not allowed by policy",
  "details": {
    "field": "args.command"
  }
}
```

| Field | Type | Required | Description |
|---|---:|---:|---|
| `code` | string | yes | Stable machine-readable error code. |
| `message` | string | yes | Human-readable diagnostic. |
| `details` | object | no | Structured context for logs and debugging. |

Initial error codes:

| Code | Status | Meaning |
|---|---|---|
| `malformed_frame` | `malformed` | Frame was not UTF-8, JSON, or a JSON object. |
| `invalid_request` | `rejected` | Required fields or field types were invalid. |
| `unsupported_protocol_version` | `rejected` | Request protocol version is unsupported. |
| `unknown_tool` | `rejected` | Guest does not recognize the requested tool. |
| `policy_denied` | `rejected` | Request violates immutable boot policy. |
| `timeout_exceeded` | `timeout` | Effective runtime limit expired. |
| `cancelled_by_host` | `cancelled` | Host cancelled the request. |
| `guest_error` | `failure` | Guest failed while preparing or running an otherwise valid request. |

## Timeouts

The effective timeout is the lower of:

- the boot policy `max_runtime_secs`
- request `limits.timeout_ms`, when provided
- any guest-internal hard safety cap

When the effective timeout expires, the guest terminates the running process tree, stops collecting output, and returns `status = "timeout"` with `error.code = "timeout_exceeded"`. The result includes captured stdout and stderr up to the effective output cap when available.

Timeout handling must not widen policy. A host cannot extend work by sending a higher request timeout than the boot policy allows.

## Cancellation

Cancellation is a control request sent over the same NDJSON stream:

```json
{
  "protocol_version": 1,
  "id": "cancel-abc123",
  "control": "cancel",
  "target_id": "abc123"
}
```

| Field | Type | Required | Description |
|---|---:|---:|---|
| `protocol_version` | integer | yes | Protocol version. |
| `id` | string | yes | Correlation id for the cancellation request itself. |
| `control` | string | yes | Must be `cancel`. |
| `target_id` | string | yes | In-flight request id to cancel. |

If the target is cancelled, the guest returns a terminal `cancelled` result for the target request and a `success` acknowledgement for the cancellation request. If the target is unknown or already terminal, the cancellation request is rejected with `error.code = "invalid_request"`.

## Output Limits

The effective output cap is the lower of the boot policy `max_output_bytes` and request `limits.max_output_bytes`, when provided.

The cap applies to the combined UTF-8 encoded stdout and stderr payload in the result frame. The guest should preserve valid UTF-8 when truncating. If output is truncated, `output_truncated` must be `true`.

The guest must bound memory use while capturing output. It must not buffer unbounded process output before applying the cap.

## Examples

### Success

Request:

```json
{"protocol_version":1,"id":"req-1","tool":"bash_command","args":{"command":"cargo","argv":["test"],"cwd":"/workspace"},"limits":{"timeout_ms":30000,"max_output_bytes":1048576}}
```

Result:

```json
{"protocol_version":1,"id":"req-1","status":"success","stdout":"test result: ok\n","stderr":"","exit_code":0,"elapsed_ms":4821,"output_truncated":false}
```

### Failure

Request:

```json
{"protocol_version":1,"id":"req-2","tool":"bash_command","args":{"command":"cargo","argv":["test"],"cwd":"/workspace"}}
```

Result:

```json
{"protocol_version":1,"id":"req-2","status":"failure","stdout":"","stderr":"error: test failed\n","exit_code":101,"elapsed_ms":2190,"output_truncated":false}
```

### Policy Rejection

Request:

```json
{"protocol_version":1,"id":"req-3","tool":"bash_command","args":{"command":"bash","argv":["-lc","curl https://example.com"],"cwd":"/workspace"}}
```

Result:

```json
{"protocol_version":1,"id":"req-3","status":"rejected","elapsed_ms":1,"error":{"code":"policy_denied","message":"command is not allowed by policy","details":{"field":"args.command","command":"bash"}}}
```

### Timeout

Request:

```json
{"protocol_version":1,"id":"req-4","tool":"bash_command","args":{"command":"cargo","argv":["test"],"cwd":"/workspace"},"limits":{"timeout_ms":1000}}
```

Result:

```json
{"protocol_version":1,"id":"req-4","status":"timeout","stdout":"running 42 tests\n","stderr":"","exit_code":null,"elapsed_ms":1000,"output_truncated":false,"error":{"code":"timeout_exceeded","message":"request exceeded effective timeout"}}
```

### Malformed Input

Request frame:

```text
{"protocol_version":1,"id":"req-5","tool":
```

Result:

```json
{"protocol_version":1,"id":null,"status":"malformed","elapsed_ms":0,"error":{"code":"malformed_frame","message":"frame is not valid JSON"}}
```

### Cancellation

Request:

```json
{"protocol_version":1,"id":"req-6","tool":"bash_command","args":{"command":"cargo","argv":["test"],"cwd":"/workspace"}}
```

Cancel request:

```json
{"protocol_version":1,"id":"cancel-req-6","control":"cancel","target_id":"req-6"}
```

Target result:

```json
{"protocol_version":1,"id":"req-6","status":"cancelled","stdout":"","stderr":"","exit_code":null,"elapsed_ms":120,"output_truncated":false,"error":{"code":"cancelled_by_host","message":"request was cancelled by host"}}
```

Cancel acknowledgement:

```json
{"protocol_version":1,"id":"cancel-req-6","status":"success","elapsed_ms":1}
```
