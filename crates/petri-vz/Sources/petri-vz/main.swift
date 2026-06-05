import Darwin
import Foundation
import Virtualization

let dispatchPortDefault: UInt32 = 7777
let workspaceTag = "workspace"
let configTag = "petri-config"

func log(_ message: String) {
    fputs("petri-vz: \(message)\n", stderr)
    fflush(stderr)
}

enum HelperState: String {
    case starting
    case ready
    case stopped
    case failed
}

struct Args {
    var instanceID: String?
    var controlSocket: String?
    var bootMode: String = "linux"
    var kernel: String?
    var initrd: String?
    var disk: String?
    var auxiliaryDisks: [String] = []
    var efiVariableStore: String?
    var workspace: String?
    var configDir: String?
    var consoleLog: String?
    var commandLine: String?
    var dispatchPort: UInt32 = dispatchPortDefault
    var guestReadyTimeoutSecs: TimeInterval = 60

    static func parse(_ values: ArraySlice<String>) throws -> Args {
        var args = Args()
        var iterator = values.makeIterator()

        while let arg = iterator.next() {
            func next(_ flag: String) throws -> String {
                guard let value = iterator.next() else {
                    throw HelperError("\(flag) requires a value")
                }
                return value
            }

            switch arg {
            case "--instance-id":
                args.instanceID = try next(arg)
            case "--control-socket":
                args.controlSocket = try next(arg)
            case "--boot-mode":
                args.bootMode = try next(arg)
            case "--kernel":
                args.kernel = try next(arg)
            case "--initrd":
                args.initrd = try next(arg)
            case "--disk":
                args.disk = try next(arg)
            case "--auxiliary-disk":
                args.auxiliaryDisks.append(try next(arg))
            case "--efi-variable-store":
                args.efiVariableStore = try next(arg)
            case "--workspace":
                args.workspace = try next(arg)
            case "--config-dir":
                args.configDir = try next(arg)
            case "--console-log":
                args.consoleLog = try next(arg)
            case "--command-line":
                args.commandLine = try next(arg)
            case "--dispatch-port":
                let value = try next(arg)
                guard let port = UInt32(value), port > 0 else {
                    throw HelperError("invalid --dispatch-port '\(value)'")
                }
                args.dispatchPort = port
            case "--guest-ready-timeout-secs":
                let value = try next(arg)
                guard let timeout = TimeInterval(value), timeout > 0 else {
                    throw HelperError("invalid --guest-ready-timeout-secs '\(value)'")
                }
                args.guestReadyTimeoutSecs = timeout
            default:
                throw HelperError("unknown argument '\(arg)'")
            }
        }

        try args.validate()
        return args
    }

    func validate() throws {
        for (name, value) in [
            ("--instance-id", instanceID),
            ("--control-socket", controlSocket),
            ("--disk", disk),
            ("--workspace", workspace),
            ("--config-dir", configDir),
            ("--console-log", consoleLog),
        ] {
            if value?.isEmpty ?? true {
                throw HelperError("\(name) is required")
            }
        }

        switch bootMode {
        case "linux":
            for (name, value) in [
                ("--kernel", kernel),
                ("--command-line", commandLine),
            ] {
                if value?.isEmpty ?? true {
                    throw HelperError("\(name) is required for linux boot mode")
                }
            }
        case "efi":
            if efiVariableStore?.isEmpty ?? true {
                throw HelperError("--efi-variable-store is required for efi boot mode")
            }
            if kernel != nil || initrd != nil || commandLine != nil {
                throw HelperError("efi boot mode does not accept --kernel, --initrd, or --command-line")
            }
        default:
            throw HelperError("invalid --boot-mode '\(bootMode)'")
        }
    }
}

struct HelperError: Error, CustomStringConvertible {
    let description: String

    init(_ description: String) {
        self.description = description
    }
}

enum JSONValue: Codable {
    case string(String)
    case number(Double)
    case bool(Bool)
    case object([String: JSONValue])
    case array([JSONValue])
    case null

    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let value = try? container.decode(Bool.self) {
            self = .bool(value)
        } else if let value = try? container.decode(Double.self) {
            self = .number(value)
        } else if let value = try? container.decode(String.self) {
            self = .string(value)
        } else if let value = try? container.decode([JSONValue].self) {
            self = .array(value)
        } else {
            self = .object(try container.decode([String: JSONValue].self))
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .string(let value):
            try container.encode(value)
        case .number(let value):
            try container.encode(value)
        case .bool(let value):
            try container.encode(value)
        case .object(let value):
            try container.encode(value)
        case .array(let value):
            try container.encode(value)
        case .null:
            try container.encodeNil()
        }
    }
}

enum HelperRequest: Decodable {
    case status
    case dispatch(JSONValue)
    case stop
    case teardown

    private enum CodingKeys: String, CodingKey {
        case type
        case request
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let type = try container.decode(String.self, forKey: .type)
        switch type {
        case "status":
            self = .status
        case "dispatch":
            self = .dispatch(try container.decode(JSONValue.self, forKey: .request))
        case "stop":
            self = .stop
        case "teardown":
            self = .teardown
        default:
            throw HelperError("unknown request type '\(type)'")
        }
    }
}

enum HelperResponse: Encodable {
    case starting
    case ready
    case stopped
    case teardownComplete
    case dispatchResult(JSONValue)
    case error(String)

    private enum CodingKeys: String, CodingKey {
        case status
        case result
        case message
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .starting:
            try container.encode("starting", forKey: .status)
        case .ready:
            try container.encode("ready", forKey: .status)
        case .stopped:
            try container.encode("stopped", forKey: .status)
        case .teardownComplete:
            try container.encode("teardown_complete", forKey: .status)
        case .dispatchResult(let result):
            try container.encode("dispatch_result", forKey: .status)
            try container.encode(result, forKey: .result)
        case .error(let message):
            try container.encode("error", forKey: .status)
            try container.encode(message, forKey: .message)
        }
    }
}

final class VMController: NSObject, VZVirtualMachineDelegate {
    private let args: Args
    private let lock = NSLock()
    private var state: HelperState = .starting
    private var failureMessage: String?
    private var virtualMachine: VZVirtualMachine?

    init(args: Args) {
        self.args = args
    }

    func start() {
        DispatchQueue.main.async {
            do {
                log("creating VM configuration")
                let vm = try self.createVirtualMachine()
                self.virtualMachine = vm
                vm.delegate = self

                log("starting VM")
                vm.start { result in
                    if case .failure(let error) = result {
                        log("failed to start VM: \(error)")
                        self.fail("failed to start VM: \(error)")
                        return
                    }

                    log("VM started; waiting for guest vsock port \(self.args.dispatchPort)")
                    DispatchQueue.global(qos: .userInitiated).async {
                        do {
                            try self.waitForGuestVsock()
                            log("guest vsock port is ready")
                            self.setState(.ready)
                        } catch {
                            log("guest vsock wait failed: \(error)")
                            self.fail("\(error)")
                        }
                    }
                }
            } catch {
                log("VM startup failed: \(error)")
                self.fail("\(error)")
            }
        }
    }

    func currentResponse() -> HelperResponse {
        lock.lock()
        defer { lock.unlock() }

        switch state {
        case .starting:
            return .starting
        case .ready:
            return .ready
        case .stopped:
            return .stopped
        case .failed:
            return .error(failureMessage ?? "VM failed")
        }
    }

    func dispatch(_ request: JSONValue) -> HelperResponse {
        lock.lock()
        let canDispatch = state == .ready
        let failure = failureMessage
        lock.unlock()

        guard canDispatch else {
            return .error(failure ?? "VM is not ready")
        }

        do {
            let requestData = try JSONEncoder().encode(request)
            let frame = requestData + Data([0x0A])
            let responseData = try sendFrameToGuest(frame)
            let result = try JSONDecoder().decode(JSONValue.self, from: responseData)
            return .dispatchResult(result)
        } catch {
            return .error("dispatch failed: \(error)")
        }
    }

    func stop() -> HelperResponse {
        guard let virtualMachine else {
            setState(.stopped)
            return .stopped
        }

        let semaphore = DispatchSemaphore(value: 0)
        var stopError: Error?
        DispatchQueue.main.async {
            virtualMachine.stop { error in
                stopError = error
                semaphore.signal()
            }
        }
        semaphore.wait()

        if let stopError {
            return .error("failed to stop VM: \(stopError)")
        }

        setState(.stopped)
        return .stopped
    }

    func teardown() -> HelperResponse {
        _ = stop()
        return .teardownComplete
    }

    func guestDidStop(_ virtualMachine: VZVirtualMachine) {
        setState(.stopped)
    }

    private func setState(_ value: HelperState) {
        lock.lock()
        state = value
        lock.unlock()
    }

    private func fail(_ message: String) {
        log("failed: \(message)")
        lock.lock()
        state = .failed
        failureMessage = message
        lock.unlock()
    }

    private func createVirtualMachine() throws -> VZVirtualMachine {
        let configuration = VZVirtualMachineConfiguration()
        configuration.platform = VZGenericPlatformConfiguration()
        configuration.cpuCount = 2
        configuration.memorySize = 1_073_741_824

        configuration.bootLoader = try bootLoader()

        var storageDevices: [VZStorageDeviceConfiguration] = [
            VZVirtioBlockDeviceConfiguration(attachment: try diskAttachment(path: args.disk!, readOnly: false))
        ]
        for auxiliaryDisk in args.auxiliaryDisks {
            storageDevices.append(
                VZVirtioBlockDeviceConfiguration(attachment: try diskAttachment(path: auxiliaryDisk, readOnly: true))
            )
        }
        configuration.storageDevices = storageDevices
        configuration.directorySharingDevices = [
            directoryShare(tag: workspaceTag, path: args.workspace!, readOnly: false),
            directoryShare(tag: configTag, path: args.configDir!, readOnly: true),
        ]
        configuration.socketDevices = [VZVirtioSocketDeviceConfiguration()]
        configuration.networkDevices = [networkDevice()]
        configuration.serialPorts = [try serialPort(path: args.consoleLog!)]
        configuration.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
        configuration.memoryBalloonDevices = [VZVirtioTraditionalMemoryBalloonDeviceConfiguration()]

        try configuration.validate()
        return VZVirtualMachine(configuration: configuration)
    }

    private func bootLoader() throws -> VZBootLoader {
        switch args.bootMode {
        case "linux":
            let bootLoader = VZLinuxBootLoader(kernelURL: URL(fileURLWithPath: args.kernel!))
            bootLoader.commandLine = args.commandLine!
            if let initrd = args.initrd {
                bootLoader.initialRamdiskURL = URL(fileURLWithPath: initrd)
            }
            return bootLoader
        case "efi":
            let bootLoader = VZEFIBootLoader()
            let storeURL = URL(fileURLWithPath: args.efiVariableStore!)
            if FileManager.default.fileExists(atPath: storeURL.path) {
                bootLoader.variableStore = VZEFIVariableStore(url: storeURL)
            } else {
                bootLoader.variableStore = try VZEFIVariableStore(creatingVariableStoreAt: storeURL, options: [])
            }
            return bootLoader
        default:
            throw HelperError("invalid boot mode '\(args.bootMode)'")
        }
    }

    private func diskAttachment(path: String, readOnly: Bool) throws -> VZDiskImageStorageDeviceAttachment {
        try VZDiskImageStorageDeviceAttachment(
            url: URL(fileURLWithPath: path),
            readOnly: readOnly
        )
    }

    private func networkDevice() -> VZVirtioNetworkDeviceConfiguration {
        let config = VZVirtioNetworkDeviceConfiguration()
        config.attachment = VZNATNetworkDeviceAttachment()
        return config
    }

    private func serialPort(path: String) throws -> VZVirtioConsoleDeviceSerialPortConfiguration {
        let config = VZVirtioConsoleDeviceSerialPortConfiguration()
        let url = URL(fileURLWithPath: path)
        config.attachment = try VZFileSerialPortAttachment(url: url, append: false)
        return config
    }

    private func directoryShare(tag: String, path: String, readOnly: Bool) -> VZVirtioFileSystemDeviceConfiguration {
        let sharedDirectory = VZSharedDirectory(url: URL(fileURLWithPath: path), readOnly: readOnly)
        let singleShare = VZSingleDirectoryShare(directory: sharedDirectory)
        let config = VZVirtioFileSystemDeviceConfiguration(tag: tag)
        config.share = singleShare
        return config
    }

    private func socketDevice() throws -> VZVirtioSocketDevice {
        guard let device = virtualMachine?.socketDevices.compactMap({ $0 as? VZVirtioSocketDevice }).first else {
            throw HelperError("VM does not expose a virtio socket device")
        }
        return device
    }

    private func waitForGuestVsock() throws {
        let deadline = Date().addingTimeInterval(args.guestReadyTimeoutSecs)
        var lastError: Error?

        while Date() < deadline {
            do {
                let connection = try connectToGuest()
                connection.close()
                return
            } catch {
                lastError = error
                Thread.sleep(forTimeInterval: 0.25)
            }
        }

        throw HelperError("guest vsock port \(args.dispatchPort) did not become ready: \(lastError.map(String.init(describing:)) ?? "unknown error")")
    }

    private func sendFrameToGuest(_ frame: Data) throws -> Data {
        let connection = try connectToGuest()
        defer { connection.close() }

        try writeAll(fd: connection.fileDescriptor, data: frame)
        return try readLine(fd: connection.fileDescriptor)
    }

    private func connectToGuest() throws -> VZVirtioSocketConnection {
        let semaphore = DispatchSemaphore(value: 0)
        var output: Result<VZVirtioSocketConnection, Error>?

        DispatchQueue.main.async {
            do {
                let device = try self.socketDevice()
                device.connect(toPort: self.args.dispatchPort) { result in
                    output = result
                    semaphore.signal()
                }
            } catch {
                output = .failure(error)
                semaphore.signal()
            }
        }
        semaphore.wait()

        return try output!.get()
    }
}

final class ControlServer {
    private let socketPath: String
    private let controller: VMController
    private let encoder = JSONEncoder()
    private let decoder = JSONDecoder()

    init(socketPath: String, controller: VMController) {
        self.socketPath = socketPath
        self.controller = controller
    }

    func run() throws {
        unlink(socketPath)
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else {
            throw POSIXError("socket")
        }
        defer { close(fd) }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        try withPathBytes(socketPath) { bytes in
            guard bytes.count < MemoryLayout.size(ofValue: addr.sun_path) else {
                throw HelperError("control socket path is too long")
            }
            withUnsafeMutableBytes(of: &addr.sun_path) { dest in
                dest.copyBytes(from: bytes)
            }
        }

        let bindResult = withUnsafePointer(to: &addr) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockaddrPointer in
                Darwin.bind(fd, sockaddrPointer, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard bindResult == 0 else {
            throw POSIXError("bind")
        }

        guard listen(fd, 128) == 0 else {
            throw POSIXError("listen")
        }

        log("control socket listening at \(socketPath)")
        while true {
            let client = accept(fd, nil, nil)
            if client < 0 {
                continue
            }

            DispatchQueue.global(qos: .userInitiated).async {
                self.handle(client)
            }
        }
    }

    private func handle(_ fd: Int32) {
        defer { close(fd) }

        do {
            let line = try readLine(fd: fd)
            let request = try decoder.decode(HelperRequest.self, from: line)
            let response: HelperResponse

            switch request {
            case .status:
                response = controller.currentResponse()
            case .dispatch(let frame):
                response = controller.dispatch(frame)
            case .stop:
                response = controller.stop()
            case .teardown:
                response = controller.teardown()
            }

            try writeResponse(response, fd: fd)
        } catch {
            let response = HelperResponse.error("\(error)")
            try? writeResponse(response, fd: fd)
        }
    }

    private func writeResponse(_ response: HelperResponse, fd: Int32) throws {
        let data = try encoder.encode(response) + Data([0x0A])
        try writeAll(fd: fd, data: data)
    }
}

struct POSIXError: Error, CustomStringConvertible {
    let operation: String
    let code: Int32

    init(_ operation: String) {
        self.operation = operation
        self.code = errno
    }

    var description: String {
        "\(operation) failed: \(String(cString: strerror(code)))"
    }
}

func withPathBytes<T>(_ path: String, _ body: ([UInt8]) throws -> T) rethrows -> T {
    var bytes = Array(path.utf8)
    bytes.append(0)
    return try body(bytes)
}

func writeAll(fd: Int32, data: Data) throws {
    try data.withUnsafeBytes { rawBuffer in
        guard let base = rawBuffer.baseAddress else {
            return
        }

        var written = 0
        while written < data.count {
            let result = Darwin.write(fd, base.advanced(by: written), data.count - written)
            if result < 0 {
                if errno == EINTR {
                    continue
                }
                throw POSIXError("write")
            }
            written += result
        }
    }
}

func readLine(fd: Int32) throws -> Data {
    var output = Data()
    var byte = UInt8(0)

    while true {
        let result = Darwin.read(fd, &byte, 1)
        if result < 0 {
            if errno == EINTR {
                continue
            }
            throw POSIXError("read")
        }
        if result == 0 {
            if output.isEmpty {
                throw HelperError("connection closed before a response frame")
            }
            return output
        }
        if byte == 0x0A {
            return output
        }
        output.append(byte)
    }
}

do {
    setbuf(stderr, nil)
    let args = try Args.parse(CommandLine.arguments.dropFirst())
    log("starting helper for instance \(args.instanceID ?? "<unknown>")")
    let controller = VMController(args: args)
    let server = ControlServer(socketPath: args.controlSocket!, controller: controller)
    DispatchQueue.global(qos: .userInitiated).async {
        do {
            try server.run()
        } catch {
            fputs("petri-vz: \(error)\n", stderr)
            exit(1)
        }
    }
    controller.start()
    RunLoop.main.run()
} catch {
    fputs("petri-vz: \(error)\n", stderr)
    exit(1)
}
