package petri

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"testing"
)

// ─── Fake runner helpers ──────────────────────────────────────────────────────

// fakeResult holds the fixed values a fakeRunner will return.
type fakeResult struct {
	stdout   []byte
	stderr   []byte
	exitCode int
	err      error
}

// fakeRunner records calls and returns a preset response.
type fakeRunner struct {
	calls  []fakeCall
	result fakeResult
}

type fakeCall struct {
	petriPath string
	args      []string
	stdin     string
}

func (f *fakeRunner) Run(ctx context.Context, petriPath string, args []string, stdin string) ([]byte, []byte, int, error) {
	f.calls = append(f.calls, fakeCall{petriPath: petriPath, args: args, stdin: stdin})
	return f.result.stdout, f.result.stderr, f.result.exitCode, f.result.err
}

func (f *fakeRunner) lastCall() fakeCall {
	return f.calls[len(f.calls)-1]
}

func newRunner(stdout string, stderr string, exitCode int) *fakeRunner {
	return &fakeRunner{
		result: fakeResult{
			stdout:   []byte(stdout),
			stderr:   []byte(stderr),
			exitCode: exitCode,
		},
	}
}

// makeResultFrameJSON builds a valid JSON ResultFrame for tests.
func makeResultFrameJSON(status string, stdout, stderr string, exitCode *int, truncated bool, errFrame *ErrorFrame) []byte {
	type frame struct {
		ProtocolVersion int         `json:"protocol_version"`
		ID              string      `json:"id"`
		Status          string      `json:"status"`
		ElapsedMs       int64       `json:"elapsed_ms"`
		Stdout          *string     `json:"stdout"`
		Stderr          *string     `json:"stderr"`
		ExitCode        *int        `json:"exit_code"`
		OutputTruncated bool        `json:"output_truncated"`
		Error           *ErrorFrame `json:"error"`
	}
	f := frame{
		ProtocolVersion: 1,
		ID:              "test-req",
		Status:          status,
		ElapsedMs:       1,
		Stdout:          &stdout,
		Stderr:          &stderr,
		ExitCode:        exitCode,
		OutputTruncated: truncated,
		Error:           errFrame,
	}
	b, _ := json.Marshal(f)
	return b
}

// ─── Create ───────────────────────────────────────────────────────────────────

func TestCreate_argv(t *testing.T) {
	fr := newRunner("sandbox-abc\n", "", 0)
	sb, err := Create(context.Background(), "base", CreateOptions{
		Workspace: "/home/user/project",
		Policy:    "./policy.toml",
		Runner:    fr.Run,
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if sb.SandboxID != "sandbox-abc" {
		t.Errorf("expected SandboxID=sandbox-abc, got %q", sb.SandboxID)
	}

	args := fr.lastCall().args
	// Check subcommand shape: ["sandbox", "create", "base", "--workspace", ..., "--policy", ...]
	if len(args) < 3 || args[0] != "sandbox" || args[1] != "create" || args[2] != "base" {
		t.Errorf("unexpected argv prefix: %v", args)
	}
	if !containsPair(args, "--workspace", "/home/user/project") {
		t.Errorf("missing --workspace in argv: %v", args)
	}
	if !containsPair(args, "--policy", "./policy.toml") {
		t.Errorf("missing --policy in argv: %v", args)
	}
}

func TestCreate_template_default(t *testing.T) {
	fr := newRunner("sandbox-xyz\n", "", 0)
	_, err := Create(context.Background(), "", CreateOptions{Runner: fr.Run})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	args := fr.lastCall().args
	if args[2] != "base" {
		t.Errorf("expected default template 'base', got %q", args[2])
	}
}

func TestCreate_optional_flags(t *testing.T) {
	fr := newRunner("sb1\n", "", 0)
	_, err := Create(context.Background(), "base", CreateOptions{
		ID:        "my-sb",
		Backend:   "macos",
		Image:     "/images/petri.img",
		Workspace: "/ws",
		Policy:    "p.toml",
		Runner:    fr.Run,
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	args := fr.lastCall().args
	if !containsPair(args, "--id", "my-sb") {
		t.Errorf("missing --id: %v", args)
	}
	if !containsPair(args, "--backend", "macos") {
		t.Errorf("missing --backend: %v", args)
	}
	if !containsPair(args, "--image", "/images/petri.img") {
		t.Errorf("missing --image: %v", args)
	}
}

func TestCreate_metadata_sorted(t *testing.T) {
	fr := newRunner("sb-meta\n", "", 0)
	_, err := Create(context.Background(), "base", CreateOptions{
		Workspace: "/ws",
		Policy:    "p.toml",
		Metadata:  map[string]string{"zoo": "1", "alpha": "2", "mid": "3"},
		Runner:    fr.Run,
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	args := fr.lastCall().args
	idx := indexOf(args, "--metadata")
	if idx < 0 || idx+1 >= len(args) {
		t.Fatalf("missing --metadata in argv: %v", args)
	}
	meta := args[idx+1]
	// Keys must appear in sorted order.
	if !strings.HasPrefix(meta, "alpha=") {
		t.Errorf("metadata not sorted: %q", meta)
	}
	parts := strings.Split(meta, ",")
	if len(parts) != 3 {
		t.Errorf("expected 3 metadata pairs, got %d: %q", len(parts), meta)
	}
}

func TestCreate_omits_metadata_when_empty(t *testing.T) {
	fr := newRunner("sb\n", "", 0)
	_, err := Create(context.Background(), "base", CreateOptions{Runner: fr.Run})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if contains(fr.lastCall().args, "--metadata") {
		t.Errorf("--metadata should be absent when empty: %v", fr.lastCall().args)
	}
}

func TestCreate_omits_optional_flags_when_empty(t *testing.T) {
	fr := newRunner("sb\n", "", 0)
	_, err := Create(context.Background(), "base", CreateOptions{Runner: fr.Run})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	args := fr.lastCall().args
	for _, flag := range []string{"--id", "--backend", "--image"} {
		if contains(args, flag) {
			t.Errorf("%s should be absent when empty: %v", flag, args)
		}
	}
}

// ─── Connect ──────────────────────────────────────────────────────────────────

func TestConnect_success(t *testing.T) {
	fr := newRunner("", "", 0)
	sb, err := Connect(context.Background(), "sb-123", ConnectOptions{Runner: fr.Run})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if sb.SandboxID != "sb-123" {
		t.Errorf("expected SandboxID=sb-123, got %q", sb.SandboxID)
	}
	args := fr.lastCall().args
	if !sliceEqual(args[:3], []string{"sandbox", "connect", "sb-123"}) {
		t.Errorf("unexpected args: %v", args)
	}
}

func TestConnect_not_found(t *testing.T) {
	fr := newRunner("", "petri: no sandbox with id 'missing'", 1)
	_, err := Connect(context.Background(), "missing", ConnectOptions{Runner: fr.Run})
	if err == nil {
		t.Fatal("expected error")
	}
	if !errors.Is(err, ErrSandboxNotFound) {
		t.Errorf("expected ErrSandboxNotFound, got %v", err)
	}
}

func TestConnect_not_running(t *testing.T) {
	fr := newRunner("", "petri: sandbox 'sb-1' is not running (state: torn_down)", 1)
	_, err := Connect(context.Background(), "sb-1", ConnectOptions{Runner: fr.Run})
	if err == nil {
		t.Fatal("expected error")
	}
	if !errors.Is(err, ErrSandboxNotReady) {
		t.Errorf("expected ErrSandboxNotReady, got %v", err)
	}
}

// ─── List ─────────────────────────────────────────────────────────────────────

func TestList_parses_json(t *testing.T) {
	listJSON := `[
		{"id":"sb-1","backend":"macos","state":"ready","metadata":{"env":"prod"}},
		{"id":"sb-2","backend":"macos","state":"running_dispatch","metadata":null}
	]`
	fr := newRunner(listJSON, "", 0)
	infos, err := List(context.Background(), ListOptions{Runner: fr.Run})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if len(infos) != 2 {
		t.Fatalf("expected 2 infos, got %d", len(infos))
	}
	if infos[0].ID != "sb-1" || infos[0].Backend != "macos" || infos[0].State != "ready" {
		t.Errorf("unexpected first entry: %+v", infos[0])
	}
	if infos[0].Metadata["env"] != "prod" {
		t.Errorf("expected metadata env=prod, got %v", infos[0].Metadata)
	}

	args := fr.lastCall().args
	if !containsPair(args, "--format", "json") {
		t.Errorf("missing --format json in argv: %v", args)
	}
}

func TestList_empty(t *testing.T) {
	fr := newRunner("[]", "", 0)
	infos, err := List(context.Background(), ListOptions{Runner: fr.Run})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if len(infos) != 0 {
		t.Errorf("expected empty slice")
	}
}

func TestList_with_state_filter(t *testing.T) {
	fr := newRunner("[]", "", 0)
	_, err := List(context.Background(), ListOptions{State: "running", Runner: fr.Run})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !containsPair(fr.lastCall().args, "--state", "running") {
		t.Errorf("missing --state running: %v", fr.lastCall().args)
	}
}

func TestList_with_limit(t *testing.T) {
	fr := newRunner("[]", "", 0)
	_, err := List(context.Background(), ListOptions{Limit: 5, Runner: fr.Run})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !containsPair(fr.lastCall().args, "--limit", "5") {
		t.Errorf("missing --limit 5: %v", fr.lastCall().args)
	}
}

// ─── Kill ─────────────────────────────────────────────────────────────────────

func TestKill_static(t *testing.T) {
	fr := newRunner("", "", 0)
	err := Kill(context.Background(), "sb-1", KillOptions{Runner: fr.Run})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	args := fr.lastCall().args
	if !sliceEqual(args, []string{"sandbox", "kill", "sb-1"}) {
		t.Errorf("unexpected args: %v", args)
	}
}

func TestKill_instance(t *testing.T) {
	fr := newRunner("", "", 0)
	sb := &Sandbox{SandboxID: "sb-kill", runner: fr.Run}
	if err := sb.Kill(context.Background()); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	args := fr.lastCall().args
	if !sliceEqual(args, []string{"sandbox", "kill", "sb-kill"}) {
		t.Errorf("unexpected args: %v", args)
	}
}

func TestKill_not_found(t *testing.T) {
	fr := newRunner("", "petri: no sandbox with id 'gone'", 1)
	err := Kill(context.Background(), "gone", KillOptions{Runner: fr.Run})
	if !errors.Is(err, ErrSandboxNotFound) {
		t.Errorf("expected ErrSandboxNotFound, got %v", err)
	}
}

// ─── Commands.Run ─────────────────────────────────────────────────────────────

func TestRun_success(t *testing.T) {
	ec := 0
	frame := makeResultFrameJSON("success", "hello\n", "", &ec, false, nil)
	fr := newRunner(string(frame), "", 0)
	sb := &Sandbox{SandboxID: "sb-run", runner: fr.Run}

	result, err := sb.Commands().Run(context.Background(), "echo hello", RunOptions{})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !result.Success() {
		t.Errorf("expected Success()=true, got false; status=%s exitCode=%v", result.Status, result.ExitCode)
	}
	if result.Stdout != "hello\n" {
		t.Errorf("expected stdout='hello\\n', got %q", result.Stdout)
	}
	if result.Stderr != "" {
		t.Errorf("expected empty stderr, got %q", result.Stderr)
	}
}

func TestRun_argv_mapping(t *testing.T) {
	ec := 0
	frame := makeResultFrameJSON("success", "out", "", &ec, false, nil)
	fr := newRunner(string(frame), "", 0)
	sb := &Sandbox{SandboxID: "sb-1", runner: fr.Run}

	_, err := sb.Commands().Run(context.Background(), "ls", RunOptions{
		Cwd:            "/workspace",
		Args:           []string{"-la"},
		Env:            map[string]string{"FOO": "bar"},
		TimeoutMs:      5000,
		MaxOutputBytes: 1024,
		RequestID:      "req-42",
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	args := fr.lastCall().args
	// subcommand prefix
	if !sliceEqual(args[:3], []string{"sandbox", "exec", "sb-1"}) {
		t.Errorf("unexpected prefix: %v", args[:3])
	}
	if !containsPair(args, "--cwd", "/workspace") {
		t.Errorf("missing --cwd: %v", args)
	}
	if !containsPair(args, "--env", "FOO=bar") {
		t.Errorf("missing --env: %v", args)
	}
	if !containsPair(args, "--timeout-ms", "5000") {
		t.Errorf("missing --timeout-ms: %v", args)
	}
	if !containsPair(args, "--max-output-bytes", "1024") {
		t.Errorf("missing --max-output-bytes: %v", args)
	}
	if !containsPair(args, "--request-id", "req-42") {
		t.Errorf("missing --request-id: %v", args)
	}
	// "--" separator before command
	sepIdx := indexOf(args, "--")
	if sepIdx < 0 {
		t.Fatalf("missing '--' separator: %v", args)
	}
	if args[sepIdx+1] != "ls" {
		t.Errorf("expected command 'ls' after --, got %q", args[sepIdx+1])
	}
	if args[sepIdx+2] != "-la" {
		t.Errorf("expected arg '-la' after command, got %q", args[sepIdx+2])
	}
}

func TestRun_stdin_piped(t *testing.T) {
	ec := 0
	frame := makeResultFrameJSON("success", "", "", &ec, false, nil)
	fr := newRunner(string(frame), "", 0)
	sb := &Sandbox{SandboxID: "sb-stdin", runner: fr.Run}

	_, err := sb.Commands().Run(context.Background(), "cat", RunOptions{Stdin: "hello world"})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if fr.lastCall().stdin != "hello world" {
		t.Errorf("expected stdin='hello world', got %q", fr.lastCall().stdin)
	}
}

func TestRun_nonzero_exit_is_result_not_error(t *testing.T) {
	ec := 2
	frame := makeResultFrameJSON("failure", "", "boom\n", &ec, false, nil)
	fr := newRunner(string(frame), "", 0) // CLI exit=0, but frame says failure
	sb := &Sandbox{SandboxID: "sb-fail", runner: fr.Run}

	result, err := sb.Commands().Run(context.Background(), "false", RunOptions{})
	if err != nil {
		t.Fatalf("non-zero exit should be a result, not error; got err=%v", err)
	}
	if result.Success() {
		t.Error("expected Success()=false")
	}
	if result.Status != "failure" {
		t.Errorf("expected status='failure', got %q", result.Status)
	}
	if result.ExitCode == nil || *result.ExitCode != 2 {
		t.Errorf("expected exit_code=2, got %v", result.ExitCode)
	}
	if result.Stderr != "boom\n" {
		t.Errorf("expected stderr='boom\\n', got %q", result.Stderr)
	}
}

// ─── CommandResult.Check ──────────────────────────────────────────────────────

func TestCheck_policy_denied_via_status(t *testing.T) {
	errFrame := &ErrorFrame{Code: "policy_denied", Message: "command not allowed"}
	frame := makeResultFrameJSON("rejected", "", "", nil, false, errFrame)
	fr := newRunner(string(frame), "", 0)
	sb := &Sandbox{SandboxID: "sb", runner: fr.Run}

	result, err := sb.Commands().Run(context.Background(), "curl", RunOptions{})
	if err != nil {
		t.Fatalf("unexpected transport error: %v", err)
	}
	checkErr := result.Check()
	if checkErr == nil {
		t.Fatal("expected Check() to return an error")
	}
	if !errors.Is(checkErr, ErrPolicyDenied) {
		t.Errorf("expected ErrPolicyDenied, got %v", checkErr)
	}
}

func TestCheck_policy_denied_via_error_code(t *testing.T) {
	errFrame := &ErrorFrame{Code: "policy_denied", Message: "not allowed"}
	// Status could be anything; the code wins.
	result := &CommandResult{
		Status: "rejected",
		Error:  errFrame,
	}
	if !errors.Is(result.Check(), ErrPolicyDenied) {
		t.Error("expected ErrPolicyDenied")
	}
}

func TestCheck_command_timeout(t *testing.T) {
	frame := makeResultFrameJSON("timeout", "", "", nil, false, nil)
	fr := newRunner(string(frame), "", 0)
	sb := &Sandbox{SandboxID: "sb", runner: fr.Run}

	result, err := sb.Commands().Run(context.Background(), "sleep 100", RunOptions{})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	checkErr := result.Check()
	if !errors.Is(checkErr, ErrCommandTimeout) {
		t.Errorf("expected ErrCommandTimeout, got %v", checkErr)
	}
}

func TestCheck_output_truncated(t *testing.T) {
	ec := 0
	frame := makeResultFrameJSON("success", strings.Repeat("x", 100), "", &ec, true, nil)
	fr := newRunner(string(frame), "", 0)
	sb := &Sandbox{SandboxID: "sb", runner: fr.Run}

	result, err := sb.Commands().Run(context.Background(), "yes", RunOptions{MaxOutputBytes: 100})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !result.OutputTruncated {
		t.Error("expected OutputTruncated=true")
	}
	checkErr := result.Check()
	if !errors.Is(checkErr, ErrOutputTruncated) {
		t.Errorf("expected ErrOutputTruncated, got %v", checkErr)
	}
}

func TestCheck_command_failed(t *testing.T) {
	ec := 1
	frame := makeResultFrameJSON("failure", "", "error msg", &ec, false, nil)
	fr := newRunner(string(frame), "", 0)
	sb := &Sandbox{SandboxID: "sb", runner: fr.Run}

	result, err := sb.Commands().Run(context.Background(), "false", RunOptions{})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	checkErr := result.Check()
	if !errors.Is(checkErr, ErrCommandFailed) {
		t.Errorf("expected ErrCommandFailed, got %v", checkErr)
	}
}

func TestCheck_success_returns_nil(t *testing.T) {
	ec := 0
	frame := makeResultFrameJSON("success", "ok", "", &ec, false, nil)
	fr := newRunner(string(frame), "", 0)
	sb := &Sandbox{SandboxID: "sb", runner: fr.Run}

	result, err := sb.Commands().Run(context.Background(), "true", RunOptions{})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if err := result.Check(); err != nil {
		t.Errorf("expected Check()=nil on success, got %v", err)
	}
}

// ─── Protocol version mismatch ────────────────────────────────────────────────

func TestRun_protocol_version_mismatch(t *testing.T) {
	// Manually build a frame with a wrong protocol version.
	type frame struct {
		ProtocolVersion int    `json:"protocol_version"`
		ID              string `json:"id"`
		Status          string `json:"status"`
		ElapsedMs       int64  `json:"elapsed_ms"`
	}
	f := frame{ProtocolVersion: 99, ID: "x", Status: "success", ElapsedMs: 1}
	b, _ := json.Marshal(f)
	fr := newRunner(string(b), "", 0)
	sb := &Sandbox{SandboxID: "sb", runner: fr.Run}

	_, err := sb.Commands().Run(context.Background(), "true", RunOptions{})
	if err == nil {
		t.Fatal("expected error for protocol version mismatch")
	}
	if !errors.Is(err, ErrProtocolVersionMismatch) {
		t.Errorf("expected ErrProtocolVersionMismatch, got %v", err)
	}
}

// ─── Stderr lifecycle errors ──────────────────────────────────────────────────

func TestRun_stderr_not_found(t *testing.T) {
	fr := newRunner("", "petri: no sandbox with id 'gone'", 1)
	sb := &Sandbox{SandboxID: "gone", runner: fr.Run}

	_, err := sb.Commands().Run(context.Background(), "ls", RunOptions{})
	if !errors.Is(err, ErrSandboxNotFound) {
		t.Errorf("expected ErrSandboxNotFound, got %v", err)
	}
}

func TestRun_stderr_not_running(t *testing.T) {
	fr := newRunner("", "petri: sandbox 'sb-1' is not running", 1)
	sb := &Sandbox{SandboxID: "sb-1", runner: fr.Run}

	_, err := sb.Commands().Run(context.Background(), "ls", RunOptions{})
	if !errors.Is(err, ErrSandboxNotReady) {
		t.Errorf("expected ErrSandboxNotReady, got %v", err)
	}
}

// ─── Reserved modules ────────────────────────────────────────────────────────

func TestReservedModules_return_not_implemented(t *testing.T) {
	sb := &Sandbox{SandboxID: "sb"}

	if !errors.Is(sb.Files(), ErrNotImplemented) {
		t.Error("Files() should return ErrNotImplemented")
	}
	if !errors.Is(sb.Git(), ErrNotImplemented) {
		t.Error("Git() should return ErrNotImplemented")
	}
	if !errors.Is(sb.Pty(), ErrNotImplemented) {
		t.Error("Pty() should return ErrNotImplemented")
	}
}

// ─── PetriError / errors.Is ──────────────────────────────────────────────────

func TestPetriError_Is_sentinel(t *testing.T) {
	cases := []struct {
		err      error
		sentinel error
	}{
		{newSandboxNotFound("x"), ErrSandboxNotFound},
		{newSandboxNotReady("x"), ErrSandboxNotReady},
		{newPolicyDenied("x"), ErrPolicyDenied},
		{newCommandTimeout("x"), ErrCommandTimeout},
		{newOutputTruncated("x"), ErrOutputTruncated},
		{newCommandFailed("x"), ErrCommandFailed},
		{newProtocolVersionMismatch("x"), ErrProtocolVersionMismatch},
		{newAuthorizationError("x"), ErrAuthorization},
	}
	for _, tc := range cases {
		if !errors.Is(tc.err, tc.sentinel) {
			t.Errorf("errors.Is(%v, %v) = false, want true", tc.err, tc.sentinel)
		}
	}
}

// ─── resolvePetriPath ─────────────────────────────────────────────────────────

func TestResolvePetriPath_explicit(t *testing.T) {
	if got := resolvePetriPath("/custom/petri"); got != "/custom/petri" {
		t.Errorf("expected /custom/petri, got %q", got)
	}
}

func TestResolvePetriPath_env(t *testing.T) {
	t.Setenv("PETRI_BIN", "/env/petri")
	if got := resolvePetriPath(""); got != "/env/petri" {
		t.Errorf("expected /env/petri, got %q", got)
	}
}

func TestResolvePetriPath_default(t *testing.T) {
	t.Setenv("PETRI_BIN", "")
	if got := resolvePetriPath(""); got != "petri" {
		t.Errorf("expected 'petri', got %q", got)
	}
}

// ─── formatMetadata ───────────────────────────────────────────────────────────

func TestFormatMetadata_sorted(t *testing.T) {
	m := map[string]string{"z": "3", "a": "1", "m": "2"}
	got := formatMetadata(m)
	want := "a=1,m=2,z=3"
	if got != want {
		t.Errorf("expected %q, got %q", want, got)
	}
}

func TestFormatMetadata_single(t *testing.T) {
	m := map[string]string{"key": "val"}
	if got := formatMetadata(m); got != "key=val" {
		t.Errorf("expected 'key=val', got %q", got)
	}
}

// ─── GetInfo / IsRunning ──────────────────────────────────────────────────────

func TestGetInfo_found(t *testing.T) {
	listJSON := `[{"id":"sb-1","backend":"macos","state":"ready","metadata":{}}]`
	fr := newRunner(listJSON, "", 0)
	sb := &Sandbox{SandboxID: "sb-1", runner: fr.Run}

	info, err := sb.GetInfo(context.Background())
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if info == nil || info.ID != "sb-1" {
		t.Errorf("expected info.ID=sb-1, got %v", info)
	}
}

func TestGetInfo_not_found(t *testing.T) {
	fr := newRunner("[]", "", 0)
	sb := &Sandbox{SandboxID: "sb-gone", runner: fr.Run}

	info, err := sb.GetInfo(context.Background())
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if info != nil {
		t.Error("expected nil info for unknown sandbox")
	}
}

func TestIsRunning_ready(t *testing.T) {
	listJSON := `[{"id":"sb-1","backend":"macos","state":"ready","metadata":{}}]`
	fr := newRunner(listJSON, "", 0)
	sb := &Sandbox{SandboxID: "sb-1", runner: fr.Run}

	running, err := sb.IsRunning(context.Background())
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !running {
		t.Error("expected IsRunning=true for state=ready")
	}
}

func TestIsRunning_dispatching(t *testing.T) {
	listJSON := `[{"id":"sb-1","backend":"macos","state":"running_dispatch","metadata":{}}]`
	fr := newRunner(listJSON, "", 0)
	sb := &Sandbox{SandboxID: "sb-1", runner: fr.Run}

	running, err := sb.IsRunning(context.Background())
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !running {
		t.Error("expected IsRunning=true for state=running_dispatch")
	}
}

func TestIsRunning_torn_down(t *testing.T) {
	fr := newRunner("[]", "", 0) // gone from list
	sb := &Sandbox{SandboxID: "sb-dead", runner: fr.Run}

	running, err := sb.IsRunning(context.Background())
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if running {
		t.Error("expected IsRunning=false for absent sandbox")
	}
}

// ─── PROTOCOL_VERSION const ───────────────────────────────────────────────────

func TestProtocolVersionConst(t *testing.T) {
	if PROTOCOL_VERSION != 1 {
		t.Errorf("expected PROTOCOL_VERSION=1, got %d", PROTOCOL_VERSION)
	}
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

// containsPair returns true if args contains flag immediately followed by value.
func containsPair(args []string, flag, value string) bool {
	for i := 0; i+1 < len(args); i++ {
		if args[i] == flag && args[i+1] == value {
			return true
		}
	}
	return false
}

// contains returns true if args contains the given string.
func contains(args []string, s string) bool {
	for _, a := range args {
		if a == s {
			return true
		}
	}
	return false
}

// indexOf returns the first index of s in args, or -1.
func indexOf(args []string, s string) int {
	for i, a := range args {
		if a == s {
			return i
		}
	}
	return -1
}

// sliceEqual returns true when a and b have identical elements.
func sliceEqual(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

// Ensure fmt is used (avoids import error if a test is removed).
var _ = fmt.Sprintf
