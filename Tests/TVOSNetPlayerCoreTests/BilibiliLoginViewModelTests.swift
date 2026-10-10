import XCTest
import TVOSNetPlayerCacheClient
@testable import TVOSNetPlayerCore

final class BilibiliLoginViewModelTests: XCTestCase {
    @MainActor
    func testMissingCredentialPathDoesNotOfferLogin() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(credentialPathConfigured: false)
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")

        XCTAssertEqual(model.status, .credentialPathMissing)
        XCTAssertFalse(model.canStartLogin)
        let loginStartCount = await client.loginStartCount
        XCTAssertEqual(loginStartCount, 0)
        XCTAssertFalse(model.statusMessage.localizedCaseInsensitiveContains("/"))
    }

    @MainActor
    func testUnsupportedCredentialStatusIsReportedWithoutStartingLogin() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [CacheServerCapability.bilibiliLoginSessions]),
            credentialStatus: .fixture()
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")

        XCTAssertEqual(model.status, .credentialStatusUnavailable)
        XCTAssertFalse(model.canStartLogin)
        let credentialStatusCallCount = await client.credentialStatusCallCount
        XCTAssertEqual(credentialStatusCallCount, 0)
    }

    @MainActor
    func testCredentialStatusFailureDoesNotOfferLogin() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            credentialStatusFails: true
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")

        XCTAssertEqual(model.status, .failed)
        XCTAssertFalse(model.canStartLogin)
    }

    @MainActor
    func testHealthyWebCookieCannotBeOverwrittenByClientLogin() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(hasWebCookie: true)
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()

        XCTAssertEqual(model.status, .authenticated)
        XCTAssertFalse(model.canStartLogin)
        let loginStartCount = await client.loginStartCount
        XCTAssertEqual(loginStartCount, 0)
    }

    @MainActor
    func testReadinessMissingAllowsFirstWebLoginWithoutCreatedCredentialFile() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
                CacheServerCapability.bilibiliCredentialReadiness,
            ]),
            credentialStatus: .fixture(
                credentialFileLoaded: false,
                state: "notConfigured",
                webCookieReadiness: .missing
            )
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")

        XCTAssertEqual(model.status, .loginRequired)
        XCTAssertTrue(model.canStartLogin)
        XCTAssertFalse(model.canStartAccessKeyLogin)
    }

    @MainActor
    func testReadyAccessKeyAllowsOnlyMissingWebLogin() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
                CacheServerCapability.bilibiliAccessKeyLogin,
                CacheServerCapability.bilibiliCredentialReadiness,
            ]),
            credentialStatus: .fixture(
                webCookieReadiness: .missing,
                accessKeyReadiness: .ready
            )
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")

        XCTAssertEqual(model.status, .authenticated)
        XCTAssertTrue(model.canStartLogin)
        XCTAssertFalse(model.canStartAccessKeyLogin)
        XCTAssertEqual(
            model.statusMessage,
            "Bilibili access-key credentials are ready; Web login is available on the cache server."
        )
        await model.startLogin()

        XCTAssertEqual(model.status, .sessionPending)
        XCTAssertFalse(model.canStartLogin)
        XCTAssertFalse(model.canStartAccessKeyLogin)
        let requestedMethod = await client.requestedMethod
        XCTAssertEqual(requestedMethod, .webQR)
        model.deactivate()
    }

    @MainActor
    func testReadyWebLoginCanCompleteThenStartMissingAccessKeyHandoff() async {
        let sessionID = "00000000-0000-4000-8000-000000000001"
        let browserSession = BilibiliLoginSession(
            id: sessionID,
            profileID: "profile-1",
            method: "accessKeyBrowser",
            state: "pending",
            message: "",
            verificationURI:
                "http://cache.local/media/login/bilibili/\(sessionID)#\(String(repeating: "a", count: 64))",
            createdAt: Date(),
            expiresAt: Date().addingTimeInterval(30)
        )
        let client = LoginClient(
            serverInfo: .fixture(
                capabilities: [
                    CacheServerCapability.bilibiliCredentialStatus,
                    CacheServerCapability.bilibiliLoginSessions,
                    CacheServerCapability.bilibiliAccessKeyLogin,
                    CacheServerCapability.bilibiliCredentialReadiness,
                ], mediaBaseURIs: ["http://cache.local/media"]),
            credentialStatus: .fixture(
                credentialFileLoaded: false,
                state: "notConfigured",
                webCookieReadiness: .loginRequired,
                accessKeyReadiness: .missing
            ),
            credentialStatusAfterLogin: .fixture(
                hasWebCookie: true,
                webCookieReadiness: .ready,
                accessKeyReadiness: .missing
            ),
            newSession: .fixture(state: "ready"),
            subsequentSession: browserSession
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "cache.local")
        XCTAssertTrue(model.canStartLogin)
        await model.startLogin()

        XCTAssertEqual(model.status, .authenticated)
        XCTAssertFalse(model.canStartLogin)
        XCTAssertTrue(model.canStartAccessKeyLogin)
        XCTAssertEqual(
            model.statusMessage,
            "Bilibili Web credentials are ready; access-key login is available on the cache server."
        )
        await model.startAccessKeyLogin()

        XCTAssertEqual(model.status, .sessionPending)
        XCTAssertEqual(model.verificationLink, browserSession.verificationURI)
        XCTAssertFalse(model.canStartLogin)
        XCTAssertFalse(model.canStartAccessKeyLogin)
        let loginStartCount = await client.loginStartCount
        let requestedMethod = await client.requestedMethod
        XCTAssertEqual(loginStartCount, 2)
        XCTAssertEqual(requestedMethod, .accessKeyBrowser)
        model.deactivate()
    }

    @MainActor
    func testReadyReadinessReusesCredentialWithoutTrustingPresenceOrLoadedFlags() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
                CacheServerCapability.bilibiliAccessKeyLogin,
                CacheServerCapability.bilibiliCredentialReadiness,
            ]),
            credentialStatus: .fixture(
                credentialFileLoaded: false,
                hasWebCookie: false,
                state: "ready",
                webCookieReadiness: .unspecified,
                accessKeyReadiness: .ready
            )
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()
        await model.startAccessKeyLogin()

        XCTAssertEqual(model.status, .authenticated)
        XCTAssertFalse(model.canStartLogin)
        XCTAssertFalse(model.canStartAccessKeyLogin)
        let loginStartCount = await client.loginStartCount
        XCTAssertEqual(loginStartCount, 0)
    }

    @MainActor
    func testUnknownCheckingAndUnavailableReadinessNeverAssumeLoginRequired() async {
        let cases: [(BilibiliCredentialReadiness, BilibiliCredentialReadiness, BilibiliLoginStatus)] = [
            (.unspecified, .unspecified, .credentialUnknown),
            (.checking, .unspecified, .checking),
            (.unavailable, .unavailable, .credentialUnavailable),
        ]
        for (web, access, expected) in cases {
            let client = LoginClient(
                serverInfo: .fixture(capabilities: [
                    CacheServerCapability.bilibiliCredentialStatus,
                    CacheServerCapability.bilibiliLoginSessions,
                    CacheServerCapability.bilibiliAccessKeyLogin,
                    CacheServerCapability.bilibiliCredentialReadiness,
                ]),
                credentialStatus: .fixture(
                    webCookieReadiness: web,
                    accessKeyReadiness: access
                )
            )
            let model = BilibiliLoginViewModel(clientFactory: { _ in client })

            await model.activate(serverAddressText: "mac-mini.local")

            XCTAssertEqual(model.status, expected)
            XCTAssertFalse(model.canStartLogin)
            XCTAssertFalse(model.canStartAccessKeyLogin)
            XCTAssertTrue(model.canCheckReadiness)
        }
    }

    @MainActor
    func testCredentialErrorOverridesPresenceAndReadinessFlags() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
                CacheServerCapability.bilibiliCredentialReadiness,
            ]),
            credentialStatus: .fixture(
                hasWebCookie: true,
                state: "error",
                webCookieReadiness: .ready,
                accessKeyReadiness: .missing
            )
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")

        XCTAssertEqual(model.status, .credentialUnavailable)
        XCTAssertFalse(model.canStartLogin)
        XCTAssertFalse(model.canStartAccessKeyLogin)
    }

    @MainActor
    func testLegacyPresenceIsNotAuthenticatedWhenCredentialFileIsUnloaded() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(credentialFileLoaded: false, hasWebCookie: true)
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")

        XCTAssertEqual(model.status, .credentialUnknown)
        XCTAssertFalse(model.canStartLogin)
    }

    @MainActor
    func testAccessKeyLoginAcceptsOnlyAdvertisedHTTPOrHTTPSOriginAndPrefix() async {
        let sessionID = "00000000-0000-4000-8000-000000000001"
        for scheme in ["http", "https"] {
            let origin = "\(scheme)://cache.local"
            let session = BilibiliLoginSession(
                id: sessionID,
                profileID: "profile-1",
                method: "accessKeyBrowser",
                state: "pending",
                message: "ignored server message",
                verificationURI: "\(origin)/media/login/bilibili/\(sessionID)#\(String(repeating: "a", count: 64))",
                createdAt: Date(),
                expiresAt: Date().addingTimeInterval(30)
            )
            let client = LoginClient(
                serverInfo: .fixture(
                    capabilities: [
                        CacheServerCapability.bilibiliCredentialStatus,
                        CacheServerCapability.bilibiliLoginSessions,
                        CacheServerCapability.bilibiliAccessKeyLogin,
                        CacheServerCapability.bilibiliCredentialReadiness,
                    ],
                    mediaBaseURIs: ["\(origin)/media"]
                ),
                credentialStatus: .fixture(accessKeyReadiness: .loginRequired),
                newSession: session
            )
            let model = BilibiliLoginViewModel(clientFactory: { _ in client })

            await model.activate(serverAddressText: "cache.local")
            XCTAssertTrue(model.canStartAccessKeyLogin)
            await model.startAccessKeyLogin()

            XCTAssertEqual(model.status, .sessionPending)
            XCTAssertEqual(model.verificationLink, session.verificationURI)
            XCTAssertEqual(model.verificationQRPayload, session.verificationURI)
            XCTAssertFalse(model.statusMessage.contains(String(repeating: "a", count: 64)))
            let requestedMethod = await client.requestedMethod
            XCTAssertEqual(requestedMethod, .accessKeyBrowser)
            model.deactivate()
        }
    }

    @MainActor
    func testAccessKeyLoginAcceptsEncodedSpaceAndUnicodePrefixes() async {
        let sessionID = "00000000-0000-4000-8000-000000000001"
        let prefixes = ["cache%20folder", "%E5%AA%92%E4%BD%93%E7%BC%93%E5%AD%98"]
        for prefix in prefixes {
            let baseURI = "https://cache.local/\(prefix)"
            let verificationURI = "\(baseURI)/login/bilibili/\(sessionID)#\(String(repeating: "a", count: 64))"
            let session = BilibiliLoginSession(
                id: sessionID,
                profileID: "profile-1",
                method: "accessKeyBrowser",
                state: "pending",
                message: "",
                verificationURI: verificationURI,
                createdAt: Date(),
                expiresAt: Date().addingTimeInterval(30)
            )
            let client = LoginClient(
                serverInfo: .fixture(
                    capabilities: [
                        CacheServerCapability.bilibiliCredentialStatus,
                        CacheServerCapability.bilibiliLoginSessions,
                        CacheServerCapability.bilibiliAccessKeyLogin,
                        CacheServerCapability.bilibiliCredentialReadiness,
                    ], mediaBaseURIs: [baseURI]),
                credentialStatus: .fixture(accessKeyReadiness: .loginRequired),
                newSession: session
            )
            let model = BilibiliLoginViewModel(clientFactory: { _ in client })

            await model.activate(serverAddressText: "cache.local")
            await model.startAccessKeyLogin()

            XCTAssertEqual(model.status, .sessionPending)
            XCTAssertEqual(model.verificationLink, verificationURI)
            model.deactivate()
        }
    }

    @MainActor
    func testAccessKeyLoginRejectsForeignMalformedAndStaleLinks() async {
        let sessionID = "00000000-0000-4000-8000-000000000002"
        let links = [
            "https://foreign.local/media/login/bilibili/\(sessionID)#capability=x",
            "https://user@cache.local/media/login/bilibili/\(sessionID)#capability=x",
            "ftp://cache.local/media/login/bilibili/\(sessionID)#capability=x",
            "http://cache.local/other/login/bilibili/\(sessionID)#capability=x",
            "http://cache.local/media/login/bilibili/\(sessionID)#",
            "http://cache.local/media/login/bilibili/\(sessionID)#abc",
            "http://cache.local/media/login/bilibili/\(sessionID)#\(String(repeating: "A", count: 64))",
            "http://cache.local/media/login/bilibili/\(sessionID)#\(String(repeating: "g", count: 64))",
            "http://cache.local/media/login/bilibili/\(sessionID)#\(String(repeating: "a", count: 63))",
            "http://cache.local/media/login/bilibili/\(sessionID)?next=/login#\(String(repeating: "a", count: 64))",
            "http://cache.local/media%2Fextra/login/bilibili/\(sessionID)#\(String(repeating: "a", count: 64))",
            "http://cache.local/media%5Cextra/login/bilibili/\(sessionID)#\(String(repeating: "a", count: 64))",
            "http://cache.local/media/%2e%2e/login/bilibili/\(sessionID)#\(String(repeating: "a", count: 64))",
            "http://cache.local/media%0Aextra/login/bilibili/\(sessionID)#\(String(repeating: "a", count: 64))",
            "http://cache.local/media%252Fextra/login/bilibili/\(sessionID)#\(String(repeating: "a", count: 64))",
            "http://cache.local/media%255Cextra/login/bilibili/\(sessionID)#\(String(repeating: "a", count: 64))",
            "http://cache.local/media/%252e%252e/login/bilibili/\(sessionID)#\(String(repeating: "a", count: 64))",
            "http://cache.local/media%250Aextra/login/bilibili/\(sessionID)#\(String(repeating: "a", count: 64))",
            "http://cache.local:8080/media/login/bilibili/\(sessionID)#\(String(repeating: "a", count: 64))",
            "http://cache.local/media/login/bilibili/00000000-0000-4000-8000-000000000003#capability=x",
        ]
        for link in links {
            let session = BilibiliLoginSession(
                id: sessionID,
                profileID: "profile-1",
                method: "accessKeyBrowser",
                state: "pending",
                message: "",
                verificationURI: link,
                createdAt: Date(),
                expiresAt: Date().addingTimeInterval(30)
            )
            let client = LoginClient(
                serverInfo: .fixture(
                    capabilities: [
                        CacheServerCapability.bilibiliCredentialStatus,
                        CacheServerCapability.bilibiliLoginSessions,
                        CacheServerCapability.bilibiliAccessKeyLogin,
                        CacheServerCapability.bilibiliCredentialReadiness,
                    ],
                    mediaBaseURIs: ["http://cache.local/media"]
                ),
                credentialStatus: .fixture(accessKeyReadiness: .loginRequired),
                newSession: session
            )
            let model = BilibiliLoginViewModel(clientFactory: { _ in client })
            await model.activate(serverAddressText: "cache.local")
            await model.startAccessKeyLogin()

            XCTAssertEqual(model.status, .failed)
            XCTAssertNil(model.verificationLink)
            XCTAssertNil(model.verificationQRPayload)
            XCTAssertFalse(model.statusMessage.contains("capability"))
            model.deactivate()
        }
    }

    @MainActor
    func testAccessKeyLoginRejectsUnsafeAdvertisedPathEncodings() async {
        let sessionID = "00000000-0000-4000-8000-000000000002"
        let unsafeBasePaths = [
            "media%2Fextra",
            "media%5Cextra",
            "media/%2e%2e/extra",
            "media%0Aextra",
            "media%252Fextra",
            "media%255Cextra",
            "media/%252e%252e/extra",
            "media%250Aextra",
        ]
        for basePath in unsafeBasePaths {
            let baseURI = "http://cache.local/\(basePath)"
            let session = BilibiliLoginSession(
                id: sessionID,
                profileID: "profile-1",
                method: "accessKeyBrowser",
                state: "pending",
                message: "",
                verificationURI: "\(baseURI)/login/bilibili/\(sessionID)#\(String(repeating: "a", count: 64))",
                createdAt: Date(),
                expiresAt: Date().addingTimeInterval(30)
            )
            let client = LoginClient(
                serverInfo: .fixture(
                    capabilities: [
                        CacheServerCapability.bilibiliCredentialStatus,
                        CacheServerCapability.bilibiliLoginSessions,
                        CacheServerCapability.bilibiliAccessKeyLogin,
                        CacheServerCapability.bilibiliCredentialReadiness,
                    ], mediaBaseURIs: [baseURI]),
                credentialStatus: .fixture(accessKeyReadiness: .loginRequired),
                newSession: session
            )
            let model = BilibiliLoginViewModel(clientFactory: { _ in client })

            await model.activate(serverAddressText: "cache.local")
            await model.startAccessKeyLogin()

            XCTAssertEqual(model.status, .failed)
            XCTAssertNil(model.verificationLink)
            model.deactivate()
        }
    }

    @MainActor
    func testSessionForDifferentProfileIsRejected() async {
        let session = BilibiliLoginSession(
            id: "00000000-0000-4000-8000-000000000004",
            profileID: "other-profile",
            method: "webQR",
            state: "pending",
            message: "",
            verificationURI: "https://passport.bilibili.com/qrcode/h5",
            createdAt: Date(),
            expiresAt: Date().addingTimeInterval(30)
        )
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            newSession: session
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()

        XCTAssertEqual(model.status, .failed)
        XCTAssertNil(model.verificationQRPayload)
    }

    @MainActor
    func testPendingSessionExposesOnlySafeBilibiliQRPayload() async {
        let session = BilibiliLoginSession(
            id: "session-1",
            profileID: "profile-1",
            method: "webQR",
            state: "pending",
            message: "private message that must not be shown",
            verificationURI: "https://passport.bilibili.com/qrcode/h5?token=short-lived",
            createdAt: Date(),
            expiresAt: Date().addingTimeInterval(30)
        )
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            newSession: session
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()

        XCTAssertEqual(model.status, .sessionPending)
        XCTAssertEqual(model.verificationQRPayload, session.verificationURI)
        XCTAssertFalse(model.canStartLogin)
        XCTAssertFalse(model.canStartAccessKeyLogin)
        XCTAssertFalse(model.statusMessage.contains("private message"))
        XCTAssertFalse(model.statusMessage.contains("token="))
        model.deactivate()
    }

    @MainActor
    func testEmptyActiveProfileUsesServerDefaultProfile() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(credentialFileLoaded: false, activeProfileID: ""),
            newSession: .fixture(state: "pending")
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")
        XCTAssertTrue(model.canStartLogin)
        await model.startLogin()

        let requestedProfileID = await client.requestedProfileID
        XCTAssertEqual(requestedProfileID, "")
        XCTAssertEqual(model.status, .sessionPending)
        model.deactivate()
    }

    @MainActor
    func testExpiredSessionCanBeRetriedWithoutReconnect() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            newSession: .fixture(state: "expired")
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()
        XCTAssertEqual(model.status, .sessionExpired)
        XCTAssertTrue(model.canStartLogin)

        await model.startLogin()

        let loginStartCount = await client.loginStartCount
        XCTAssertEqual(loginStartCount, 2)
        XCTAssertEqual(model.status, .sessionExpired)
    }

    @MainActor
    func testLoginStartCanOutlastOrdinaryStatusTimeout() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            startDelay: .milliseconds(75)
        )
        let model = BilibiliLoginViewModel(
            operationTimeout: .milliseconds(20),
            pollInterval: .seconds(2),
            clientFactory: { _ in client }
        )

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()

        XCTAssertEqual(model.status, .sessionPending)
        XCTAssertNotNil(model.verificationQRPayload)
        model.deactivate()
    }

    @MainActor
    func testReactivatingSameServerResumesLiveSessionWithoutClearingQR() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            newSession: .fixture(state: "pending")
        )
        let model = BilibiliLoginViewModel(
            pollInterval: .milliseconds(10),
            clientFactory: { _ in client }
        )

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()
        let payload = model.verificationQRPayload
        let initialCredentialStatusCallCount = await client.credentialStatusCallCount
        model.deactivate()

        await model.activate(serverAddressText: "mac-mini.local:50051")
        for _ in 0..<50 {
            if await client.loginPollCount > 0 {
                break
            }
            try? await Task.sleep(for: .milliseconds(10))
        }

        let resumedCredentialStatusCallCount = await client.credentialStatusCallCount
        let loginPollCount = await client.loginPollCount
        XCTAssertEqual(model.status, .sessionPending)
        XCTAssertEqual(model.verificationQRPayload, payload)
        XCTAssertEqual(resumedCredentialStatusCallCount, initialCredentialStatusCallCount)
        XCTAssertGreaterThan(loginPollCount, 0)
        model.deactivate()
    }

    @MainActor
    func testReadySessionRefreshesRedactedCredentialStatus() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            credentialStatusAfterLogin: .fixture(hasWebCookie: true),
            newSession: .fixture(state: "pending"),
            polledSession: .fixture(state: "ready")
        )
        let model = BilibiliLoginViewModel(
            pollInterval: .milliseconds(10),
            clientFactory: { _ in client }
        )

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()
        for _ in 0..<50 {
            if model.status == .authenticated {
                break
            }
            try? await Task.sleep(for: .milliseconds(10))
        }

        let credentialStatusCallCount = await client.credentialStatusCallCount
        XCTAssertEqual(model.status, .authenticated)
        XCTAssertEqual(model.statusMessage, "Bilibili credentials are configured on the cache server.")
        XCTAssertEqual(credentialStatusCallCount, 2)
        model.deactivate()
    }

    @MainActor
    func testNonBilibiliVerificationURLIsRejected() async {
        let session = BilibiliLoginSession(
            id: "session-1",
            profileID: "profile-1",
            method: "webQR",
            state: "pending",
            message: "",
            verificationURI: "https://example.com/login?token=secret",
            createdAt: Date(),
            expiresAt: Date().addingTimeInterval(30)
        )
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            newSession: session
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")
        await model.startLogin()

        XCTAssertEqual(model.status, .failed)
        XCTAssertNil(model.verificationQRPayload)
        XCTAssertFalse(model.statusMessage.contains("secret"))
        model.deactivate()
    }

    @MainActor
    func testChangingServerWhileStartIsPendingAllowsRetry() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            startDelay: .milliseconds(100)
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "first.local")
        let oldStart = Task { await model.startLogin() }
        for _ in 0..<50 {
            if await client.loginStartCount > 0 { break }
            try? await Task.sleep(for: .milliseconds(2))
        }
        XCTAssertTrue(model.isStartingLogin)
        XCTAssertFalse(model.canStartLogin)
        XCTAssertFalse(model.canStartAccessKeyLogin)

        await model.activate(serverAddressText: "")
        XCTAssertEqual(model.status, .disconnected)
        XCTAssertFalse(model.canStartLogin)
        XCTAssertFalse(model.canStartAccessKeyLogin)
        await model.activate(serverAddressText: "second.local")
        XCTAssertEqual(model.status, .loginRequired)
        XCTAssertFalse(model.isStartingLogin)
        XCTAssertTrue(model.canStartLogin)

        await oldStart.value
        XCTAssertEqual(model.status, .loginRequired)
        XCTAssertTrue(model.canStartLogin)
        model.deactivate()
    }

    @MainActor
    func testLeavingViewWhileStartIsPendingAllowsRetryOnReturn() async {
        let client = LoginClient(
            serverInfo: .fixture(capabilities: [
                CacheServerCapability.bilibiliCredentialStatus,
                CacheServerCapability.bilibiliLoginSessions,
            ]),
            credentialStatus: .fixture(),
            startDelay: .milliseconds(100)
        )
        let model = BilibiliLoginViewModel(clientFactory: { _ in client })

        await model.activate(serverAddressText: "mac-mini.local")
        let oldStart = Task { await model.startLogin() }
        for _ in 0..<50 {
            if await client.loginStartCount > 0 { break }
            try? await Task.sleep(for: .milliseconds(2))
        }
        XCTAssertTrue(model.isStartingLogin)

        model.deactivate()
        XCTAssertFalse(model.isStartingLogin)
        await model.activate(serverAddressText: "mac-mini.local")
        XCTAssertEqual(model.status, .loginRequired)
        XCTAssertTrue(model.canStartLogin)

        await oldStart.value
        XCTAssertEqual(model.status, .loginRequired)
        XCTAssertTrue(model.canStartLogin)
        model.deactivate()
    }
}

private actor LoginClient: CacheControlClient {
    let serverInfo: CacheServerSummary
    let credentialStatus: BilibiliCredentialStatus
    let credentialStatusAfterLogin: BilibiliCredentialStatus?
    let credentialStatusFails: Bool
    let newSession: BilibiliLoginSession
    let subsequentSession: BilibiliLoginSession?
    let polledSession: BilibiliLoginSession?
    let startDelay: Duration
    private(set) var credentialStatusCallCount = 0
    private(set) var loginStartCount = 0
    private(set) var loginPollCount = 0
    private(set) var requestedProfileID = ""
    private(set) var requestedMethod: BilibiliLoginMethod = .webQR

    init(
        serverInfo: CacheServerSummary,
        credentialStatus: BilibiliCredentialStatus,
        credentialStatusAfterLogin: BilibiliCredentialStatus? = nil,
        credentialStatusFails: Bool = false,
        newSession: BilibiliLoginSession = .fixture(state: "pending"),
        subsequentSession: BilibiliLoginSession? = nil,
        polledSession: BilibiliLoginSession? = nil,
        startDelay: Duration = .zero
    ) {
        self.serverInfo = serverInfo
        self.credentialStatus = credentialStatus
        self.credentialStatusAfterLogin = credentialStatusAfterLogin
        self.credentialStatusFails = credentialStatusFails
        self.newSession = newSession
        self.subsequentSession = subsequentSession
        self.polledSession = polledSession
        self.startDelay = startDelay
    }

    func getServerInfo() async throws -> CacheServerSummary {
        serverInfo
    }

    func getBilibiliCredentialStatus() async throws -> BilibiliCredentialStatus {
        credentialStatusCallCount += 1
        if credentialStatusFails {
            throw LoginClientError.unused
        }
        if credentialStatusCallCount > 1, let credentialStatusAfterLogin {
            return credentialStatusAfterLogin
        }
        return credentialStatus
    }

    func startBilibiliLoginSession(
        profileID: String,
        method: BilibiliLoginMethod
    ) async throws -> BilibiliLoginSession {
        let session = loginStartCount == 0 ? newSession : subsequentSession ?? newSession
        loginStartCount += 1
        requestedProfileID = profileID
        requestedMethod = method
        if startDelay > .zero {
            try await Task.sleep(for: startDelay)
        }
        return session
    }

    func getBilibiliLoginSession(id: String) async throws -> BilibiliLoginSession {
        loginPollCount += 1
        return polledSession ?? newSession
    }

    func listCacheRoots() async throws -> [CacheRoot] {
        throw LoginClientError.unused
    }

    func listLibraryItemsPage(
        pageToken: String,
        pageSize: Int,
        searchText: String?
    ) async throws -> CacheLibraryItemsPage {
        throw LoginClientError.unused
    }

    func getPlaybackSource(itemID: String, variantID: String) async throws -> CachePlaybackSource {
        throw LoginClientError.unused
    }

    func deleteLibraryItem(id: String) async throws -> Bool {
        throw LoginClientError.unused
    }

    func getTask(id: String) async throws -> CacheTask {
        throw LoginClientError.unused
    }

    func watchTasks(ids: [String]) async -> AsyncThrowingStream<CacheTask, Error> {
        AsyncThrowingStream { $0.finish() }
    }

    func cancelTask(id: String) async throws -> CacheTask {
        throw LoginClientError.unused
    }

    func createBilibiliPlaybackTask(
        urlOrID: String,
        options: BilibiliPlaybackTaskOptions
    ) async throws -> CacheTask {
        throw LoginClientError.unused
    }
}

private enum LoginClientError: Error {
    case unused
}

private extension CacheServerSummary {
    static func fixture(capabilities: [String], mediaBaseURIs: [String] = []) -> CacheServerSummary {
        CacheServerSummary(
            id: "server-1",
            name: "Cache server",
            version: "1.0.0",
            mediaBaseURIs: mediaBaseURIs,
            capabilities: capabilities
        )
    }
}

private extension BilibiliCredentialStatus {
    static func fixture(
        credentialPathConfigured: Bool = true,
        credentialFileLoaded: Bool = true,
        hasWebCookie: Bool = false,
        activeProfileID: String = "profile-1",
        state: String = "ready",
        webCookieReadiness: BilibiliCredentialReadiness = .unspecified,
        accessKeyReadiness: BilibiliCredentialReadiness = .unspecified
    ) -> BilibiliCredentialStatus {
        BilibiliCredentialStatus(
            state: state,
            message: "sensitive path must remain hidden",
            credentialPathConfigured: credentialPathConfigured,
            credentialFileLoaded: credentialFileLoaded,
            hasWebCookie: hasWebCookie,
            hasAccessKey: false,
            hasTVAccessKey: false,
            restrictedArea: "",
            restrictedPlayURLProxyCount: 0,
            restrictedAPIProxyCount: 0,
            checkedAt: Date(),
            activeProfileID: activeProfileID,
            defaultProfileID: "profile-1",
            profileCount: 1,
            webCookieReadiness: webCookieReadiness,
            accessKeyReadiness: accessKeyReadiness
        )
    }
}

private extension BilibiliLoginSession {
    static func fixture(state: String) -> BilibiliLoginSession {
        BilibiliLoginSession(
            id: "session-1",
            profileID: "profile-1",
            method: "webQR",
            state: state,
            message: "",
            verificationURI: "https://passport.bilibili.com/qrcode/h5",
            createdAt: Date(),
            expiresAt: Date().addingTimeInterval(30)
        )
    }
}
