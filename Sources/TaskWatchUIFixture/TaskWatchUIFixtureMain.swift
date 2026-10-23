import Dispatch
import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2Posix
import Darwin

@main
private enum TaskWatchUIFixtureMain {
    static func main() async {
        do {
            let configuration = try FixtureConfiguration.parse(arguments: Array(CommandLine.arguments.dropFirst()))
            try FileManager.default.removeItemIfPresent(at: configuration.readyFile)

            let metrics = FixtureMetricsStore(fileURL: configuration.metricsFile)
            try await metrics.initialize()

            try await withGRPCServer(
                transport: .http2NIOPosix(
                    address: .ipv4(host: "127.0.0.1", port: 0),
                    transportSecurity: .plaintext
                ),
                services: [TaskWatchUIFixtureService(metrics: metrics, mediaURL: configuration.mediaURL)]
            ) { server in
                let address = try await server.transport.listeningAddress
                guard let port = address.ipv4?.port, (1...65_535).contains(port) else {
                    throw FixtureProcessError.invalidListeningAddress
                }
                try writeReadinessFile(configuration.readyFile, port: port)
                await FixtureTerminationWaiter().wait()
                try FileManager.default.removeItemIfPresent(at: configuration.readyFile)
            }
        } catch {
            FileHandle.standardError.write(Data("task-watch-ui-fixture failed to start or stop.\n".utf8))
            exit(EXIT_FAILURE)
        }
    }

    private static func writeReadinessFile(_ url: URL, port: Int) throws {
        let data = try JSONSerialization.data(
            withJSONObject: ["schema_version": 1, "server_address": "127.0.0.1:\(port)"],
            options: [.sortedKeys]
        )
        guard data.count <= 1_024 else {
            throw FixtureProcessError.readinessFileTooLarge
        }
        try data.write(to: url, options: .atomic)
    }
}

private enum FixtureProcessError: Error {
    case invalidListeningAddress
    case readinessFileTooLarge
}

private extension FileManager {
    func removeItemIfPresent(at url: URL) throws {
        if fileExists(atPath: url.path) {
            try removeItem(at: url)
        }
    }
}

private final class FixtureTerminationWaiter: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<Void, Never>?
    private var signalSources: [DispatchSourceSignal] = []
    private var finished = false

    func wait() async {
        await withCheckedContinuation { continuation in
            lock.lock()
            self.continuation = continuation
            for signalNumber in [SIGINT, SIGTERM] {
                Darwin.signal(signalNumber, SIG_IGN)
                let source = DispatchSource.makeSignalSource(signal: signalNumber, queue: .global())
                source.setEventHandler { [weak self] in
                    self?.finish()
                }
                signalSources.append(source)
                source.resume()
            }
            lock.unlock()
        }
    }

    private func finish() {
        lock.lock()
        guard !finished else {
            lock.unlock()
            return
        }
        finished = true
        let continuation = self.continuation
        self.continuation = nil
        let sources = signalSources
        signalSources.removeAll()
        lock.unlock()

        sources.forEach { $0.cancel() }
        continuation?.resume()
    }
}
