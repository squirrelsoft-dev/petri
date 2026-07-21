package petri

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"os/exec"
	"strings"
)

// Runner is a function type that executes a command with the given args and
// stdin, returning stdout, stderr, the exit code, and any execution error.
// An execution error is distinct from a non-zero exit code: the error
// represents a failure to run the process at all (binary not found, etc.).
// A non-zero exit code with a nil error means the process ran but failed.
type Runner func(ctx context.Context, petriPath string, args []string, stdin string) (stdout, stderr []byte, exitCode int, err error)

// defaultRunner is the production Runner that shells out to the real petri
// binary, resolving it via PetriPath option → PETRI_BIN env → "petri" on PATH.
func defaultRunner(ctx context.Context, petriPath string, args []string, stdin string) ([]byte, []byte, int, error) {
	cmd := exec.CommandContext(ctx, petriPath, args...)
	if stdin != "" {
		cmd.Stdin = strings.NewReader(stdin)
	}
	var stdoutBuf, stderrBuf bytes.Buffer
	cmd.Stdout = &stdoutBuf
	cmd.Stderr = &stderrBuf

	err := cmd.Run()
	exitCode := 0
	if err != nil {
		if exitErr, ok := err.(*exec.ExitError); ok {
			// A non-zero exit is not an execution error: report it via
			// exitCode and return a nil error below.
			exitCode = exitErr.ExitCode()
		} else {
			return nil, nil, 0, fmt.Errorf("petri: failed to run binary %q: %w", petriPath, err)
		}
	}
	return stdoutBuf.Bytes(), stderrBuf.Bytes(), exitCode, nil
}

// resolvePetriPath returns the petri binary path following the resolution order:
// explicit petriPath option → PETRI_BIN env var → "petri" (rely on PATH).
func resolvePetriPath(petriPath string) string {
	if petriPath != "" {
		return petriPath
	}
	if v := os.Getenv("PETRI_BIN"); v != "" {
		return v
	}
	return "petri"
}

// mapLifecycleError inspects the stderr from a non-zero CLI exit and returns
// the appropriate typed error. The petri CLI emits "petri: <message>" on
// stderr; we strip that prefix before matching.
func mapLifecycleError(stderr []byte) error {
	msg := strings.TrimSpace(string(stderr))
	// Strip the "petri: " prefix that the CLI adds to error messages.
	clean := strings.TrimPrefix(msg, "petri: ")

	switch {
	case strings.Contains(clean, "no sandbox with id"):
		return newSandboxNotFound(clean)
	case strings.Contains(clean, "not running"):
		return newSandboxNotReady(clean)
	case strings.Contains(clean, "protocol_version_mismatch"), strings.Contains(clean, "protocol version mismatch"):
		return newProtocolVersionMismatch(clean)
	default:
		if clean == "" {
			clean = "petri CLI exited non-zero with no error message"
		}
		return newPetriError(nil, "cli_error", clean)
	}
}
