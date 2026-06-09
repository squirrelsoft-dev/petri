// Package petri provides a Go client for the Petri sandbox system.
//
// It mirrors the E2B-style Sandbox SDK described in docs/sdk-api.md and
// implemented as a reference in crates/petri/src/sdk.rs. The client is a
// thin wrapper over the petri CLI; all SDK calls shell out to
// "petri sandbox ..." and parse the CLI's JSON output.
//
// # Quick start
//
//	sb, err := petri.Create(ctx, "base", petri.CreateOptions{
//	    Workspace: ".",
//	    Policy:    "./policy.toml",
//	})
//	if err != nil {
//	    log.Fatal(err)
//	}
//	defer sb.Kill(ctx)
//
//	result, err := sb.Commands().Run(ctx, "cargo test", petri.RunOptions{})
//	if err != nil {
//	    log.Fatal(err)
//	}
//	if !result.Success() {
//	    log.Printf("command failed: %s", result.Stderr)
//	}
package petri

import (
	"context"
	"encoding/json"
	"fmt"
	"sort"
	"strings"
)

// PROTOCOL_VERSION is the ResultFrame protocol version this client expects.
const PROTOCOL_VERSION = 1

// ─── Option structs ──────────────────────────────────────────────────────────

// CreateOptions holds parameters for Sandbox.Create.
type CreateOptions struct {
	// ID is an explicit sandbox id; generated when empty.
	ID string
	// Workspace is the host directory to mount into the sandbox.
	Workspace string
	// Policy is the path to the policy file.
	Policy string
	// Backend selects the VM backend (defaults to "macos").
	Backend string
	// Image overrides the backend's base image path.
	Image string
	// Metadata is free-form key/value data persisted with the instance.
	Metadata map[string]string

	// PetriPath overrides the petri binary location (see resolvePetriPath).
	PetriPath string
	// Runner replaces the default exec-based runner; useful for tests.
	Runner Runner
}

// ConnectOptions holds parameters for Sandbox.Connect.
type ConnectOptions struct {
	PetriPath string
	Runner    Runner
}

// ListOptions holds parameters for Sandbox.List.
type ListOptions struct {
	// State filters by lifecycle state (e.g. "running").
	State string
	// Metadata filters by metadata key=value pairs.
	Metadata map[string]string
	// Limit caps the number of results returned.
	Limit int

	PetriPath string
	Runner    Runner
}

// KillOptions holds parameters for the static Sandbox.Kill.
type KillOptions struct {
	PetriPath string
	Runner    Runner
}

// RunOptions holds parameters for Commands.Run.
type RunOptions struct {
	// Cwd sets the working directory inside the sandbox.
	Cwd string
	// Args are extra arguments appended after the command.
	Args []string
	// Env sets environment variable overrides.
	Env map[string]string
	// Stdin is piped to the process's standard input.
	Stdin string
	// TimeoutMs sets a per-request wall-clock timeout.
	TimeoutMs int64
	// MaxOutputBytes limits captured output before truncation.
	MaxOutputBytes int64
	// RequestID is an explicit correlation id; generated when empty.
	RequestID string
}

// ─── Result types ────────────────────────────────────────────────────────────

// SandboxInfo is a parsed entry from "petri sandbox list --format json".
type SandboxInfo struct {
	ID       string            `json:"id"`
	Backend  string            `json:"backend"`
	State    string            `json:"state"`
	Metadata map[string]string `json:"metadata"`
}

// ErrorFrame is the structured error block inside a ResultFrame.
type ErrorFrame struct {
	Code    string          `json:"code"`
	Message string          `json:"message"`
	Details json.RawMessage `json:"details,omitempty"`
}

// resultFrame is the internal JSON shape of "petri sandbox exec" stdout.
type resultFrame struct {
	ProtocolVersion int        `json:"protocol_version"`
	ID              string     `json:"id"`
	Status          string     `json:"status"`
	ElapsedMs       int64      `json:"elapsed_ms"`
	Stdout          *string    `json:"stdout"`
	Stderr          *string    `json:"stderr"`
	ExitCode        *int       `json:"exit_code"`
	OutputTruncated bool       `json:"output_truncated"`
	Error           *ErrorFrame `json:"error"`
}

// CommandResult is the SDK-facing view of a dispatch ResultFrame.
// A non-success status or non-zero exit code is NOT an error return from Run;
// callers who want errors for those cases should call Check().
type CommandResult struct {
	// Status is the dispatch outcome: "success", "failure", "rejected",
	// "timeout", "cancelled", or "malformed".
	Status string
	// ExitCode is the process exit code, or nil when not applicable.
	ExitCode *int
	// Stdout is the captured standard output (never nil).
	Stdout string
	// Stderr is the captured standard error (never nil).
	Stderr string
	// OutputTruncated is true when output was cut at MaxOutputBytes.
	OutputTruncated bool
	// Error is the structured error block for non-success statuses.
	Error *ErrorFrame
}

// Success returns true when status is "success" and ExitCode is 0.
func (r *CommandResult) Success() bool {
	return r.Status == "success" && r.ExitCode != nil && *r.ExitCode == 0
}

// Check returns the first applicable typed error in contract order:
//
//  1. ErrPolicyDenied  — status "rejected" or error.code "policy_denied"
//  2. ErrCommandTimeout — status "timeout"
//  3. ErrCommandFailed  — status "failure" (non-zero exit)
//  4. ErrOutputTruncated — OutputTruncated is true (may accompany success)
//
// Returns nil on a clean success.
func (r *CommandResult) Check() error {
	if r.Status == "rejected" || (r.Error != nil && r.Error.Code == "policy_denied") {
		msg := "command rejected by policy"
		if r.Error != nil && r.Error.Message != "" {
			msg = r.Error.Message
		}
		return newPolicyDenied(msg)
	}
	if r.Status == "timeout" {
		msg := "command timed out"
		if r.Error != nil && r.Error.Message != "" {
			msg = r.Error.Message
		}
		return newCommandTimeout(msg)
	}
	if r.Status == "failure" {
		exitStr := ""
		if r.ExitCode != nil {
			exitStr = fmt.Sprintf(" (exit %d)", *r.ExitCode)
		}
		return newCommandFailed("command failed" + exitStr)
	}
	if r.OutputTruncated {
		return newOutputTruncated("output was truncated")
	}
	return nil
}

// ─── Sandbox ─────────────────────────────────────────────────────────────────

// Sandbox is a handle to a running Petri sandbox.
type Sandbox struct {
	// SandboxID is the unique identifier for this sandbox.
	SandboxID string

	petriPath string
	runner    Runner
}

func (s *Sandbox) run(ctx context.Context) Runner {
	if s.runner != nil {
		return s.runner
	}
	return defaultRunner
}

// Commands returns the Commands module for this sandbox.
func (s *Sandbox) Commands() *Commands {
	return &Commands{sandbox: s}
}

// Files is reserved for v1 and always returns ErrNotImplemented.
func (s *Sandbox) Files() error {
	return ErrNotImplemented
}

// Git is reserved for v1 and always returns ErrNotImplemented.
func (s *Sandbox) Git() error {
	return ErrNotImplemented
}

// Pty is reserved for v1 and always returns ErrNotImplemented.
func (s *Sandbox) Pty() error {
	return ErrNotImplemented
}

// Kill tears this sandbox down.
func (s *Sandbox) Kill(ctx context.Context) error {
	return Kill(ctx, s.SandboxID, KillOptions{PetriPath: s.petriPath, Runner: s.runner})
}

// GetInfo returns the current lifecycle handle for this sandbox.
func (s *Sandbox) GetInfo(ctx context.Context) (*SandboxInfo, error) {
	infos, err := List(ctx, ListOptions{PetriPath: s.petriPath, Runner: s.runner})
	if err != nil {
		return nil, err
	}
	for i := range infos {
		if infos[i].ID == s.SandboxID {
			return &infos[i], nil
		}
	}
	return nil, nil
}

// IsRunning returns true when the sandbox state is "ready" or "running_dispatch".
func (s *Sandbox) IsRunning(ctx context.Context) (bool, error) {
	info, err := s.GetInfo(ctx)
	if err != nil {
		return false, err
	}
	if info == nil {
		return false, nil
	}
	return info.State == "ready" || info.State == "running_dispatch", nil
}

// ─── Static constructors ─────────────────────────────────────────────────────

// Create creates a new sandbox from the given template and options, returning
// a handle to it. Template defaults to "base" when empty.
func Create(ctx context.Context, template string, opts CreateOptions) (*Sandbox, error) {
	if template == "" {
		template = "base"
	}
	petriPath := resolvePetriPath(opts.PetriPath)
	runner := opts.Runner
	if runner == nil {
		runner = defaultRunner
	}

	args := []string{"sandbox", "create", template}

	// Required options — include even if empty; CLI will error with a helpful message.
	if opts.Workspace != "" {
		args = append(args, "--workspace", opts.Workspace)
	}
	if opts.Policy != "" {
		args = append(args, "--policy", opts.Policy)
	}

	// Optional flags — include only when non-empty.
	if opts.ID != "" {
		args = append(args, "--id", opts.ID)
	}
	if opts.Backend != "" {
		args = append(args, "--backend", opts.Backend)
	}
	if opts.Image != "" {
		args = append(args, "--image", opts.Image)
	}
	if len(opts.Metadata) > 0 {
		args = append(args, "--metadata", formatMetadata(opts.Metadata))
	}

	stdout, stderr, exitCode, err := runner(ctx, petriPath, args, "")
	if err != nil {
		return nil, err
	}
	if exitCode != 0 {
		return nil, mapLifecycleError(stderr)
	}

	id := strings.TrimSpace(string(stdout))
	if id == "" {
		return nil, newPetriError(nil, "cli_error", "petri sandbox create returned empty id")
	}
	return &Sandbox{SandboxID: id, petriPath: petriPath, runner: opts.Runner}, nil
}

// Connect attaches to an existing running sandbox by id.
// It errors if the sandbox does not exist or is not currently running.
func Connect(ctx context.Context, id string, opts ConnectOptions) (*Sandbox, error) {
	petriPath := resolvePetriPath(opts.PetriPath)
	runner := opts.Runner
	if runner == nil {
		runner = defaultRunner
	}

	args := []string{"sandbox", "connect", id}
	_, stderr, exitCode, err := runner(ctx, petriPath, args, "")
	if err != nil {
		return nil, err
	}
	if exitCode != 0 {
		return nil, mapLifecycleError(stderr)
	}
	return &Sandbox{SandboxID: id, petriPath: petriPath, runner: opts.Runner}, nil
}

// List returns all sandboxes matching the given options.
func List(ctx context.Context, opts ListOptions) ([]SandboxInfo, error) {
	petriPath := resolvePetriPath(opts.PetriPath)
	runner := opts.Runner
	if runner == nil {
		runner = defaultRunner
	}

	args := []string{"sandbox", "list", "--format", "json"}
	if opts.State != "" {
		args = append(args, "--state", opts.State)
	}
	if len(opts.Metadata) > 0 {
		args = append(args, "--metadata", formatMetadata(opts.Metadata))
	}
	if opts.Limit > 0 {
		args = append(args, "--limit", fmt.Sprintf("%d", opts.Limit))
	}

	stdout, stderr, exitCode, err := runner(ctx, petriPath, args, "")
	if err != nil {
		return nil, err
	}
	if exitCode != 0 {
		return nil, mapLifecycleError(stderr)
	}

	var infos []SandboxInfo
	if err := json.Unmarshal(stdout, &infos); err != nil {
		return nil, newPetriError(nil, "parse_error",
			fmt.Sprintf("failed to parse sandbox list JSON: %v", err))
	}
	return infos, nil
}

// Kill tears down a sandbox by id without requiring a handle to it.
func Kill(ctx context.Context, id string, opts KillOptions) error {
	petriPath := resolvePetriPath(opts.PetriPath)
	runner := opts.Runner
	if runner == nil {
		runner = defaultRunner
	}

	args := []string{"sandbox", "kill", id}
	_, stderr, exitCode, err := runner(ctx, petriPath, args, "")
	if err != nil {
		return err
	}
	if exitCode != 0 {
		return mapLifecycleError(stderr)
	}
	return nil
}

// ─── Commands ────────────────────────────────────────────────────────────────

// Commands is the commands module for a sandbox.
type Commands struct {
	sandbox *Sandbox
}

// Run executes a command inside the sandbox and returns a typed CommandResult.
//
// A non-success status (rejected, timeout, failure) or a non-zero exit code
// does NOT produce an error return — that is a normal result. The caller may
// call result.Check() to convert those outcomes to typed errors.
//
// Run returns a non-nil error only for transport/usage failures: binary not
// found, malformed JSON output, or protocol version mismatch.
func (c *Commands) Run(ctx context.Context, command string, opts RunOptions) (*CommandResult, error) {
	s := c.sandbox
	petriPath := resolvePetriPath(s.petriPath)
	runner := s.run(ctx)

	args := []string{"sandbox", "exec", s.SandboxID}

	if opts.Cwd != "" {
		args = append(args, "--cwd", opts.Cwd)
	}
	if len(opts.Env) > 0 {
		args = append(args, "--env", formatMetadata(opts.Env))
	}
	if opts.TimeoutMs > 0 {
		args = append(args, "--timeout-ms", fmt.Sprintf("%d", opts.TimeoutMs))
	}
	if opts.MaxOutputBytes > 0 {
		args = append(args, "--max-output-bytes", fmt.Sprintf("%d", opts.MaxOutputBytes))
	}
	if opts.RequestID != "" {
		args = append(args, "--request-id", opts.RequestID)
	}

	// Stop option parsing before the command so a command beginning with "-"
	// is not interpreted as a flag.
	args = append(args, "--", command)
	args = append(args, opts.Args...)

	stdout, stderr, exitCode, err := runner(ctx, petriPath, args, opts.Stdin)
	if err != nil {
		return nil, err
	}
	if exitCode != 0 {
		return nil, mapLifecycleError(stderr)
	}

	var frame resultFrame
	if err := json.Unmarshal(stdout, &frame); err != nil {
		return nil, newPetriError(nil, "parse_error",
			fmt.Sprintf("failed to parse exec result JSON: %v", err))
	}

	if frame.ProtocolVersion != PROTOCOL_VERSION {
		return nil, newProtocolVersionMismatch(
			fmt.Sprintf("expected protocol_version %d, got %d", PROTOCOL_VERSION, frame.ProtocolVersion))
	}

	result := &CommandResult{
		Status:          frame.Status,
		ExitCode:        frame.ExitCode,
		OutputTruncated: frame.OutputTruncated,
		Error:           frame.Error,
	}
	if frame.Stdout != nil {
		result.Stdout = *frame.Stdout
	}
	if frame.Stderr != nil {
		result.Stderr = *frame.Stderr
	}

	// Check for authorization errors embedded in the error frame.
	if frame.Error != nil && frame.Error.Code == "capability_denied" {
		return nil, newAuthorizationError(frame.Error.Message)
	}

	return result, nil
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

// formatMetadata serialises a map[string]string into a "k=v,k=v,..." string
// with keys sorted for determinism (stable test assertions).
func formatMetadata(m map[string]string) string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)

	parts := make([]string, 0, len(keys))
	for _, k := range keys {
		parts = append(parts, k+"="+m[k])
	}
	return strings.Join(parts, ",")
}
