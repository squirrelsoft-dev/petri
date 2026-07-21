package petri

import "errors"

// Sentinel errors for use with errors.Is.
var (
	ErrSandboxNotFound         = errors.New("sandbox not found")
	ErrSandboxNotReady         = errors.New("sandbox not ready")
	ErrPolicyDenied            = errors.New("policy denied")
	ErrCommandTimeout          = errors.New("command timeout")
	ErrOutputTruncated         = errors.New("output truncated")
	ErrCommandFailed           = errors.New("command failed")
	ErrProtocolVersionMismatch = errors.New("protocol version mismatch")
	ErrAuthorization           = errors.New("authorization error")
	ErrNotImplemented          = errors.New("not implemented in v1")
)

// PetriError is the base error type for all Petri SDK errors.
// It wraps a sentinel error so errors.Is works across the hierarchy,
// and carries an optional code and human-readable message.
type PetriError struct {
	// sentinel is the typed sentinel (e.g. ErrSandboxNotFound) used by errors.Is.
	sentinel error
	// Code is the machine-readable error code from the protocol, if any.
	Code string
	// Message is the human-readable description.
	Message string
}

func (e *PetriError) Error() string {
	if e.Code != "" {
		return e.Code + ": " + e.Message
	}
	return e.Message
}

// Unwrap returns the sentinel so errors.Is works transitively.
func (e *PetriError) Unwrap() error {
	return e.sentinel
}

// newPetriError creates a PetriError wrapping the given sentinel.
func newPetriError(sentinel error, code, message string) *PetriError {
	return &PetriError{sentinel: sentinel, Code: code, Message: message}
}

// newSandboxNotFound returns a PetriError matching ErrSandboxNotFound.
func newSandboxNotFound(message string) error {
	return newPetriError(ErrSandboxNotFound, "sandbox_not_found", message)
}

// newSandboxNotReady returns a PetriError matching ErrSandboxNotReady.
func newSandboxNotReady(message string) error {
	return newPetriError(ErrSandboxNotReady, "sandbox_not_ready", message)
}

// newPolicyDenied returns a PetriError matching ErrPolicyDenied.
func newPolicyDenied(message string) error {
	return newPetriError(ErrPolicyDenied, "policy_denied", message)
}

// newCommandTimeout returns a PetriError matching ErrCommandTimeout.
func newCommandTimeout(message string) error {
	return newPetriError(ErrCommandTimeout, "command_timeout", message)
}

// newOutputTruncated returns a PetriError matching ErrOutputTruncated.
func newOutputTruncated(message string) error {
	return newPetriError(ErrOutputTruncated, "output_truncated", message)
}

// newCommandFailed returns a PetriError matching ErrCommandFailed.
func newCommandFailed(message string) error {
	return newPetriError(ErrCommandFailed, "command_failed", message)
}

// newProtocolVersionMismatch returns a PetriError matching ErrProtocolVersionMismatch.
func newProtocolVersionMismatch(message string) error {
	return newPetriError(ErrProtocolVersionMismatch, "protocol_version_mismatch", message)
}

// newAuthorizationError returns a PetriError matching ErrAuthorization.
func newAuthorizationError(message string) error {
	return newPetriError(ErrAuthorization, "authorization_error", message)
}
