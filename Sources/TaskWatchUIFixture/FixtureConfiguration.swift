import Foundation

package enum FixtureScenario: String, CaseIterable, Codable, Sendable {
    case automatic
    case manual

    package var taskID: String {
        switch self {
        case .automatic: "ui-e2e-automatic"
        case .manual: "ui-e2e-manual"
        }
    }

    package var source: String {
        switch self {
        case .automatic: "BV1QtjA6BEB8"
        case .manual: "BV1uW4y1s7zN"
        }
    }

    package var title: String {
        switch self {
        case .automatic: "Automatic recovery fixture"
        case .manual: "Manual recovery fixture"
        }
    }

    package var resolutionSessionID: String {
        "\(taskID)-resolution"
    }

    package var resultID: String {
        "\(taskID)-result"
    }

    package var candidateToken: String {
        "\(taskID)-candidate"
    }
}

package struct FixtureConfiguration: Sendable {
    package let readyFile: URL
    package let metricsFile: URL
    package let mediaURL: URL

    package static func parse(arguments: [String]) throws -> Self {
        guard arguments.count == 6 else {
            throw FixtureConfigurationError.invalidArguments
        }

        var options: [String: String] = [:]
        var index = 0
        while index < arguments.count {
            let key = arguments[index]
            guard ["--ready-file", "--metrics-file", "--media-url"].contains(key),
                options[key] == nil,
                index + 1 < arguments.count
            else {
                throw FixtureConfigurationError.invalidArguments
            }
            options[key] = arguments[index + 1]
            index += 2
        }

        guard let readyPath = options["--ready-file"],
            let metricsPath = options["--metrics-file"],
            let mediaAddress = options["--media-url"]
        else {
            throw FixtureConfigurationError.invalidArguments
        }

        let readyFile = try validatedOutputFile(readyPath)
        let metricsFile = try validatedOutputFile(metricsPath)
        guard readyFile != metricsFile else {
            throw FixtureConfigurationError.invalidOutputPaths
        }

        return Self(
            readyFile: readyFile,
            metricsFile: metricsFile,
            mediaURL: try validatedMediaURL(mediaAddress)
        )
    }

    package static func validatedMediaURL(_ value: String) throws -> URL {
        guard value.utf8.count <= 2_048,
            let components = URLComponents(string: value),
            components.scheme?.lowercased() == "http",
            let host = components.host?.lowercased(),
            ["127.0.0.1", "localhost", "::1"].contains(host),
            let port = components.port,
            (1...65_535).contains(port),
            components.user == nil,
            components.password == nil,
            components.fragment == nil,
            components.query == nil,
            Self.isSupportedMediaPath(components.percentEncodedPath),
            let url = components.url
        else {
            throw FixtureConfigurationError.invalidMediaURL
        }
        return url
    }

    private static func isSupportedMediaPath(_ path: String) -> Bool {
        if path.lowercased().hasSuffix(".mp4") {
            return true
        }

        let segments = path.split(separator: "/", omittingEmptySubsequences: false)
        guard segments.count == 4,
            segments[0].isEmpty,
            segments[1] == "media",
            segments[2].hasPrefix("local.default."),
            segments[3] == "original"
        else {
            return false
        }

        let token = segments[2].dropFirst("local.default.".count)
        let unpadded = token.prefix { $0 != "=" }
        let padding = token.dropFirst(unpadded.count)
        guard !unpadded.isEmpty,
            padding.count <= 2,
            padding.allSatisfy({ $0 == "=" }),
            unpadded.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "-" || $0 == "_") }),
            unpadded.count % 4 != 1,
            padding.isEmpty || token.count % 4 == 0
        else {
            return false
        }

        let standardBase64 =
            unpadded
            .replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        let requiredPadding = (4 - unpadded.count % 4) % 4
        return Data(base64Encoded: standardBase64 + String(repeating: "=", count: requiredPadding)) != nil
    }

    private static func validatedOutputFile(_ path: String) throws -> URL {
        guard path.utf8.count <= 4_096,
            path.hasPrefix("/"),
            !path.utf8.contains(0),
            !path.split(separator: "/", omittingEmptySubsequences: false).contains("..")
        else {
            throw FixtureConfigurationError.invalidOutputPath
        }

        let fileURL = URL(fileURLWithPath: path).standardizedFileURL
        guard fileURL.path != "/" else {
            throw FixtureConfigurationError.invalidOutputPath
        }

        let fileManager = FileManager.default
        let parentURL = fileURL.deletingLastPathComponent()
        var isDirectory: ObjCBool = false
        guard fileManager.fileExists(atPath: parentURL.path, isDirectory: &isDirectory), isDirectory.boolValue else {
            throw FixtureConfigurationError.invalidOutputPath
        }

        if fileManager.fileExists(atPath: fileURL.path) {
            guard let attributes = try? fileManager.attributesOfItem(atPath: fileURL.path),
                attributes[.type] as? FileAttributeType == .typeRegular
            else {
                throw FixtureConfigurationError.invalidOutputPath
            }
        }

        return fileURL
    }
}

package enum FixtureConfigurationError: Error {
    case invalidArguments
    case invalidMediaURL
    case invalidOutputPath
    case invalidOutputPaths
}

package enum FixtureSourceResolver {
    package static func scenario(for input: String) throws -> FixtureScenario {
        let value = input.trimmingCharacters(in: .whitespacesAndNewlines)
        if let scenario = FixtureScenario.allCases.first(where: { $0.source == value }) {
            return scenario
        }

        let urlValue: String
        if value.hasPrefix("//") {
            urlValue = "https:\(value)"
        } else if value.contains("://") {
            urlValue = value
        } else {
            urlValue = "https://\(value)"
        }

        guard let components = URLComponents(string: urlValue),
            ["http", "https"].contains(components.scheme?.lowercased() ?? ""),
            let host = components.host?.lowercased(),
            ["bilibili.com", "www.bilibili.com", "m.bilibili.com"].contains(host),
            components.user == nil,
            components.password == nil,
            components.fragment == nil
        else {
            throw FixtureSourceError.unsupportedInput
        }

        let segments = components.path.split(separator: "/")
        guard let videoIndex = segments.firstIndex(of: "video"),
            segments.indices.contains(videoIndex + 1),
            let scenario = FixtureScenario.allCases.first(where: {
                $0.source == segments[videoIndex + 1]
            })
        else {
            throw FixtureSourceError.unsupportedInput
        }
        return scenario
    }
}

package enum FixtureSourceError: Error {
    case unsupportedInput
}
