module github.com/squirrelsoft/petri-go

// The `go` directive is the SDK's minimum supported version for consumers and
// is deliberately conservative. `toolchain` pins the version we build and scan
// with, which is what osv-scanner reports stdlib CVEs against.
go 1.22

toolchain go1.26.3
