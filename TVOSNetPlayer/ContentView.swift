import SwiftUI
import AVKit
import CoreImage
import CoreImage.CIFilterBuiltins
import TVOSNetPlayerCore
import TVOSNetPlayerCacheClient

struct ContentView: View {
    @ObservedObject var model: PlayerViewModel
    @ObservedObject var cacheModel: CacheLibraryViewModel
    @ObservedObject var discoveryModel: CacheServerDiscoveryViewModel
    @ObservedObject var bilibiliModel: BilibiliTaskViewModel
    @ObservedObject var bilibiliLoginModel: BilibiliLoginViewModel
    @State private var pendingDeleteItem: CacheLibraryItem?
    @State private var isLoginQRPresented = false
    @State private var isResolverSettingsPresented = false
    @State private var isAutoDiscoveryConnecting = false
    @State private var failedAutoDiscoveryServerIDs: Set<String> = []
    @FocusState private var focusedControl: FocusedControl?
    private let autoDiscoveryRetryDelay: Duration = .seconds(30)

    private enum FocusedControl: Hashable {
        case cacheServerField
        case refreshButton
        case cacheSearchField
        case cacheSearchButton
        case cacheLoadMoreButton
        case bilibiliField
        case bilibiliSubmitButton
        case bilibiliCandidateLoadMoreButton
        case bilibiliTaskResultsLoadMoreButton
        case bilibiliTaskResultsRetryButton
        case urlField
        case playButton
    }

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()

            VStack(alignment: .leading, spacing: 28) {
                VStack(alignment: .leading, spacing: 10) {
                    Text("TVOS Net Player")
                        .font(.largeTitle.weight(.semibold))
                    Text("\(model.statusMessage) \(cacheModel.statusMessage) \(bilibiliModel.statusMessage)")
                        .foregroundStyle(.secondary)
                }

                HStack(alignment: .top, spacing: 34) {
                    cacheControls
                        .frame(width: 500)

                    VStack(alignment: .leading, spacing: 22) {
                        manualStreamControls
                        playbackArea
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            .padding(.horizontal, 72)
            .padding(.vertical, 58)
        }
        .onAppear {
            discoveryModel.start()
            Task {
                await bilibiliLoginModel.activate(serverAddressText: cacheModel.serverAddressText)
            }
            focusedControl =
                cacheModel.serverAddressText.isEmpty
                ? .cacheServerField
                : (model.streamURLText.isEmpty ? .refreshButton : .playButton)
            Task {
                await autoConnectDiscoveredServerIfNeeded()
            }
        }
        .onDisappear {
            bilibiliLoginModel.deactivate()
        }
        .onChange(of: cacheModel.serverAddressText) { _, newValue in
            Task {
                await bilibiliLoginModel.activate(serverAddressText: newValue)
            }
        }
        .onChange(of: bilibiliLoginModel.verificationQRPayload) { _, payload in
            isLoginQRPresented = payload != nil
        }
        .onChange(of: discoveryModel.discoveredServers) { _, _ in
            Task {
                await autoConnectDiscoveredServerIfNeeded()
            }
        }
        .onChange(of: model.playbackProgressStatusRefreshRequestID) { _, _ in
            Task {
                await refreshPlaybackProgressStatus()
            }
        }
        .confirmationDialog(
            "Delete Cached Video?",
            isPresented: Binding(
                get: { pendingDeleteItem != nil },
                set: { isPresented in
                    if !isPresented {
                        pendingDeleteItem = nil
                    }
                }
            ),
            titleVisibility: .visible,
            presenting: pendingDeleteItem
        ) { item in
            Button("Delete", role: .destructive) {
                confirmDeleteCachedItem(item)
            }
            Button("Cancel", role: .cancel) {
                pendingDeleteItem = nil
            }
        } message: { item in
            Text("Delete \(item.displayTitle) from the cache server.")
        }
        .sheet(isPresented: $isLoginQRPresented) {
            bilibiliLoginQRSheet
        }
        .sheet(isPresented: $isResolverSettingsPresented) {
            if let endpoint = cacheModel.resolverSettingsEndpoint {
                ResolverSettingsSheet(endpoint: endpoint)
            }
        }
    }

    private var cacheControls: some View {
        VStack(alignment: .leading, spacing: 20) {
            Text(cacheModel.serverName)
                .font(.title2.weight(.semibold))

            VStack(alignment: .leading, spacing: 10) {
                TextField(
                    "mac-mini.local:50051 or https://cache.example.com",
                    text: $cacheModel.serverAddressText
                )
                .keyboardType(.URL)
                .submitLabel(.go)
                .onSubmit {
                    Task {
                        await cacheModel.refresh()
                    }
                }
                .focused($focusedControl, equals: .cacheServerField)

                if let errorMessage = cacheModel.errorMessage {
                    Text(errorMessage)
                        .font(.callout)
                        .foregroundStyle(.red)
                }
            }

            HStack(spacing: 14) {
                Button {
                    Task {
                        await cacheModel.refresh()
                    }
                } label: {
                    Label(cacheModel.isLoading ? "Loading" : "Refresh", systemImage: "arrow.clockwise")
                }
                .buttonStyle(.borderedProminent)
                .disabled(!cacheModel.canRefresh)
                .focused($focusedControl, equals: .refreshButton)

                if cacheModel.supportsResolverSettingsWrite,
                    cacheModel.resolverSettingsEndpoint != nil
                {
                    Button {
                        isResolverSettingsPresented = true
                    } label: {
                        Label("Settings", systemImage: "gearshape")
                    }
                    .buttonStyle(.bordered)
                }
            }

            discoveryControls

            bilibiliLoginControls

            if !cacheModel.cacheRoots.isEmpty {
                VStack(alignment: .leading, spacing: 6) {
                    ForEach(cacheModel.cacheRoots) { root in
                        CacheRootRow(root: root)
                    }
                }
            }

            if let hlsCacheSummary = cacheModel.hlsCacheSummary {
                Label(hlsCacheSummary, systemImage: "externaldrive.badge.timemachine")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .lineLimit(3)
            }

            if !cacheModel.hlsCacheStatusBadges.isEmpty {
                VStack(alignment: .leading, spacing: 6) {
                    ForEach(cacheModel.hlsCacheStatusBadges) { badge in
                        CacheStatusBadgeRow(badge: badge)
                    }
                }
            }

            HStack(spacing: 12) {
                TextField("Search cached videos", text: $cacheModel.searchText)
                    .textContentType(.none)
                    .submitLabel(.search)
                    .onSubmit {
                        Task {
                            await cacheModel.refresh()
                        }
                    }
                    .focused($focusedControl, equals: .cacheSearchField)

                Button {
                    Task {
                        await cacheModel.refresh()
                    }
                } label: {
                    Label(cacheModel.hasPendingSearch ? "Search" : "Reload", systemImage: "magnifyingglass")
                }
                .buttonStyle(.bordered)
                .disabled(!cacheModel.canRefresh)
                .focused($focusedControl, equals: .cacheSearchButton)
            }

            Divider()

            bilibiliControls

            Divider()

            if cacheModel.items.isEmpty {
                VStack(spacing: 12) {
                    ZStack {
                        RoundedRectangle(cornerRadius: 8)
                            .fill(.white.opacity(0.08))
                        Text("No cached videos")
                            .foregroundStyle(.secondary)
                    }
                    .frame(maxWidth: .infinity, minHeight: 220)

                    cacheLoadMoreButton
                }
            } else {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 12) {
                        ForEach(cacheModel.items) { item in
                            HStack(alignment: .center, spacing: 10) {
                                Button {
                                    Task {
                                        await playCachedItem(item)
                                    }
                                } label: {
                                    CacheLibraryRow(item: item)
                                }
                                .buttonStyle(.bordered)
                                .disabled(
                                    cacheModel.isLoading
                                        || cacheModel.deletingItemIDs.contains(item.id)
                                        || !item.hasPlayableVariant
                                )

                                Button {
                                    pendingDeleteItem = item
                                } label: {
                                    Label(
                                        cacheModel.deletingItemIDs.contains(item.id) ? "Deleting" : "Delete",
                                        systemImage: "trash"
                                    )
                                }
                                .buttonStyle(.bordered)
                                .disabled(!cacheModel.canDelete(item))
                            }
                        }

                        cacheLoadMoreButton
                    }
                }
            }
        }
    }

    private var bilibiliLoginControls: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Bilibili Login")
                .font(.headline)
            Text(bilibiliLoginModel.statusMessage)
                .font(.callout)
                .foregroundStyle(.secondary)
                .lineLimit(2)

            if bilibiliLoginModel.canStartLogin {
                Button {
                    Task {
                        await bilibiliLoginModel.startLogin()
                    }
                } label: {
                    Label(
                        bilibiliLoginModel.isStartingLogin ? "Starting Login" : "Sign In with QR Code",
                        systemImage: "qrcode"
                    )
                }
                .buttonStyle(.bordered)
                .disabled(bilibiliLoginModel.isStartingLogin)
            } else if bilibiliLoginModel.verificationQRPayload != nil {
                Button {
                    isLoginQRPresented = true
                } label: {
                    Label("Show Login QR Code", systemImage: "qrcode")
                }
                .buttonStyle(.bordered)
            }
        }
    }

    private var bilibiliLoginQRSheet: some View {
        VStack(spacing: 24) {
            Text("Bilibili Web Login")
                .font(.title2.weight(.semibold))
            Text(bilibiliLoginModel.statusMessage)
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .lineLimit(2)

            if let payload = bilibiliLoginModel.verificationQRPayload {
                BilibiliLoginQRCode(payload: payload)
                    .frame(width: 360, height: 360)
                    .accessibilityLabel("Bilibili Web login QR code")
            }

            Button("Close") {
                isLoginQRPresented = false
            }
            .buttonStyle(.borderedProminent)
        }
        .padding(32)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.black.ignoresSafeArea())
        .presentationDetents([.large])
    }

    @ViewBuilder
    private var discoveryControls: some View {
        if discoveryModel.isSearching || discoveryModel.errorMessage != nil || !discoveryModel.discoveredServers.isEmpty
        {
            VStack(alignment: .leading, spacing: 8) {
                Text(discoveryModel.statusMessage)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)

                ForEach(discoveryModel.discoveredServers.prefix(4)) { server in
                    Button {
                        Task {
                            await selectDiscoveredServer(server)
                        }
                    } label: {
                        Label {
                            VStack(alignment: .leading, spacing: 2) {
                                Text(server.displayName)
                                Text(server.detailText)
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                        } icon: {
                            Image(systemName: "network")
                        }
                    }
                    .buttonStyle(.bordered)
                    .disabled(cacheModel.isLoading)
                }
            }
        }
    }

    @ViewBuilder
    private var cacheLoadMoreButton: some View {
        if cacheModel.hasMoreItems {
            Button {
                Task {
                    await cacheModel.loadMore()
                }
            } label: {
                Label(
                    cacheModel.isLoadingMore ? "Loading More" : "Load More",
                    systemImage: "chevron.down.circle"
                )
                .frame(maxWidth: .infinity)
            }
            .buttonStyle(.bordered)
            .disabled(!cacheModel.canLoadMore)
            .focused($focusedControl, equals: .cacheLoadMoreButton)
        }
    }

    private var bilibiliControls: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Bilibili")
                .font(.title3.weight(.semibold))

            TextField("BV1xx411c7mD or Bilibili URL", text: $bilibiliModel.sourceText)
                .keyboardType(.URL)
                .submitLabel(.go)
                .onSubmit {
                    Task {
                        await bilibiliModel.submit(serverAddressText: cacheModel.serverAddressText)
                    }
                }
                .focused($focusedControl, equals: .bilibiliField)

            Picker("Mode", selection: $bilibiliModel.submissionMode) {
                ForEach(BilibiliTaskSubmissionMode.allCases) { mode in
                    Text(mode.title).tag(mode)
                }
            }
            .pickerStyle(.segmented)

            HStack(spacing: 10) {
                TextField("Quality", text: $bilibiliModel.qualityPreference)
                    .textContentType(.none)

                if bilibiliModel.submissionMode == .playback {
                    TextField("Codec", text: $bilibiliModel.encodingPreference)
                        .textContentType(.none)
                }

                TextField("Audio", text: $bilibiliModel.audioLanguagePreference)
                    .textContentType(.none)
            }

            if bilibiliModel.submissionMode == .playback {
                bilibiliPlaybackPolicyControls
            }

            if bilibiliModel.submissionMode == .download {
                bilibiliDownloadOptions
            }

            if let errorMessage = bilibiliModel.errorMessage {
                Text(errorMessage)
                    .font(.callout)
                    .foregroundStyle(.red)
            }

            if let notice = bilibiliModel.fetchNotice {
                BilibiliFetchNoticeRow(notice: notice)
            }

            if shouldShowStandaloneBilibiliNoticeAction {
                bilibiliReResolveButton
            }

            if bilibiliModel.isWaitingForCandidateSelection {
                Picker("Selection Mode", selection: $bilibiliModel.candidateSelectionMode) {
                    ForEach(bilibiliModel.availableCandidateSelectionModes) { mode in
                        Text(mode.title).tag(mode)
                    }
                }
                .pickerStyle(.segmented)

                if bilibiliModel.candidateSelectionMode == .range {
                    HStack(spacing: 10) {
                        Picker("From", selection: $bilibiliModel.rangeStartCandidateID) {
                            ForEach(bilibiliModel.resolvedCandidates) { candidate in
                                Text(candidate.title).tag(Optional(candidate.selectionID))
                            }
                        }

                        Picker("To", selection: $bilibiliModel.rangeEndCandidateID) {
                            ForEach(bilibiliModel.resolvedCandidates) { candidate in
                                Text(candidate.title).tag(Optional(candidate.selectionID))
                            }
                        }
                    }
                }

                if let selectionSummary = bilibiliModel.candidateSelectionSummary {
                    Text(selectionSummary)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }

                Text(candidatePaginationSummary)
                    .font(.caption)
                    .foregroundStyle(.secondary)

                HStack(spacing: 12) {
                    bilibiliReResolveButton

                    Button {
                        bilibiliModel.clearResolvedCandidateSelection()
                    } label: {
                        Label("Clear Selection", systemImage: "xmark.circle")
                    }
                    .buttonStyle(.bordered)
                    .disabled(!bilibiliModel.canClearCandidateSelection)
                }

                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 8) {
                        ForEach(bilibiliModel.resolvedCandidates) { candidate in
                            Button {
                                bilibiliModel.chooseCandidate(candidate)
                            } label: {
                                HStack(spacing: 10) {
                                    Image(
                                        systemName: bilibiliModel.isCandidateSelected(candidate)
                                            ? "checkmark.circle.fill"
                                            : "circle"
                                    )
                                    VStack(alignment: .leading, spacing: 2) {
                                        Text(candidate.title)
                                            .lineLimit(1)
                                        if !candidate.subtitle.isEmpty {
                                            Text(candidate.subtitle)
                                                .font(.caption)
                                                .foregroundStyle(.secondary)
                                                .lineLimit(1)
                                        }
                                    }
                                }
                                .frame(maxWidth: .infinity, alignment: .leading)
                            }
                            .buttonStyle(.bordered)
                            .disabled(bilibiliModel.candidateSelectionMode == .all)
                        }
                    }
                }
                .frame(maxHeight: 260)

                if bilibiliModel.hasMoreResolvedCandidates || bilibiliModel.isLoadingMoreCandidates {
                    Button {
                        Task {
                            await bilibiliModel.loadMoreResolvedCandidates(
                                serverAddressText: cacheModel.serverAddressText
                            )
                        }
                    } label: {
                        Label(
                            bilibiliModel.isLoadingMoreCandidates ? "Loading Candidates" : "Load More Candidates",
                            systemImage: bilibiliModel.isLoadingMoreCandidates
                                ? "hourglass"
                                : "chevron.down.circle"
                        )
                    }
                    .buttonStyle(.bordered)
                    .disabled(!bilibiliModel.hasMoreResolvedCandidates || bilibiliModel.isLoadingMoreCandidates)
                    .focused($focusedControl, equals: .bilibiliCandidateLoadMoreButton)
                }
            }

            if bilibiliModel.currentTask != nil || bilibiliModel.isSubmitting || bilibiliModel.isResolving {
                VStack(alignment: .leading, spacing: 8) {
                    ProgressView(value: bilibiliModel.progress)
                    Text(bilibiliModel.statusMessage)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                    if let summary = bilibiliModel.activePlaybackPolicySummary {
                        Text(summary)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                    }
                    if let badge = bilibiliModel.progressiveCacheStatusBadge {
                        Label(badge.label, systemImage: badge.systemImage)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    bilibiliTaskResults
                }
            }

            HStack(spacing: 12) {
                Button {
                    Task {
                        await bilibiliModel.submit(serverAddressText: cacheModel.serverAddressText)
                    }
                } label: {
                    Label(bilibiliModel.submitButtonTitle, systemImage: "plus.circle.fill")
                }
                .buttonStyle(.borderedProminent)
                .disabled(!bilibiliModel.canSubmit)
                .focused($focusedControl, equals: .bilibiliSubmitButton)

                Button {
                    Task {
                        await playBilibiliTask()
                    }
                } label: {
                    Label("Play", systemImage: "play.fill")
                }
                .disabled(!bilibiliModel.canPlay)

                Button {
                    Task {
                        await bilibiliModel.cancel(serverAddressText: cacheModel.serverAddressText)
                    }
                } label: {
                    Label(bilibiliModel.isCancelling ? "Cancelling" : "Cancel", systemImage: "xmark.circle")
                }
                .disabled(!bilibiliModel.canCancel)
            }

            HStack(spacing: 12) {
                Button {
                    Task {
                        await bilibiliModel.retry(serverAddressText: cacheModel.serverAddressText)
                    }
                } label: {
                    Label("Retry", systemImage: "arrow.clockwise")
                }
                .disabled(!bilibiliModel.canRetry)

                Button {
                    bilibiliModel.clearTask()
                } label: {
                    Label("Clear", systemImage: "trash")
                }
                .disabled(!bilibiliModel.canClear)
            }
        }
    }

    private var bilibiliDownloadOptions: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 16) {
                Toggle("Subtitles", isOn: $bilibiliModel.downloadSubtitles)
                Toggle("Danmaku", isOn: $bilibiliModel.downloadDanmaku)
                Toggle("Cover", isOn: $bilibiliModel.downloadCover)
            }

            Picker("Subtitle AI", selection: $bilibiliModel.subtitleAIPolicy) {
                ForEach(bilibiliModel.availableSubtitleAIPolicies, id: \.self) { policy in
                    Text(policy.title).tag(policy)
                }
            }
            .disabled(!bilibiliModel.downloadSubtitles)

            HStack(spacing: 16) {
                ForEach(bilibiliModel.availableDanmakuFormats, id: \.self) { format in
                    Toggle(
                        format.title,
                        isOn: Binding(
                            get: { bilibiliModel.isDanmakuFormatSelected(format) },
                            set: { bilibiliModel.setDanmakuFormat(format, selected: $0) }
                        )
                    )
                    .disabled(!bilibiliModel.downloadDanmaku)
                }
            }
        }
    }

    private var shouldShowStandaloneBilibiliNoticeAction: Bool {
        bilibiliModel.fetchNotice?.actionTitle == "Re-resolve"
            && bilibiliModel.canReResolve
            && !bilibiliModel.isWaitingForCandidateSelection
    }

    private var bilibiliReResolveButton: some View {
        Button {
            Task {
                await bilibiliModel.reResolve(serverAddressText: cacheModel.serverAddressText)
            }
        } label: {
            Label("Re-resolve", systemImage: "arrow.triangle.2.circlepath")
        }
        .buttonStyle(.bordered)
        .disabled(!bilibiliModel.canReResolve)
    }

    private var manualStreamControls: some View {
        VStack(alignment: .leading, spacing: 10) {
            TextField("http://192.168.1.10:8080/video.mp4", text: $model.streamURLText)
                .keyboardType(.URL)
                .submitLabel(.go)
                .onSubmit(loadManualStream)
                .focused($focusedControl, equals: .urlField)

            if let validationMessage = model.validationMessage {
                Text(validationMessage)
                    .font(.callout)
                    .foregroundStyle(.red)
            }
            if let playbackProgressReportingMessage = model.playbackProgressReportingMessage {
                Text(playbackProgressReportingMessage)
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }

            HStack(spacing: 18) {
                Button(action: loadManualStream) {
                    Label("Play", systemImage: "play.fill")
                }
                .buttonStyle(.borderedProminent)
                .focused($focusedControl, equals: .playButton)

                Button(action: stopManualStream) {
                    Label("Stop", systemImage: "stop.fill")
                }
                .disabled(model.player == nil)

                Button(action: clearManualStream) {
                    Label("Clear", systemImage: "xmark.circle")
                }
                .disabled(!model.canClear)
            }
        }
    }

    private var bilibiliPlaybackPolicyControls: some View {
        HStack(spacing: 10) {
            Picker("Transcode", selection: $bilibiliModel.playbackTranscodingPreference) {
                ForEach(bilibiliModel.availableTranscodingPreferences, id: \.self) { preference in
                    Text(preference.title).tag(preference)
                }
            }
            .pickerStyle(.menu)

            Picker("Variant", selection: $bilibiliModel.playbackCompatibleVariantPreference) {
                ForEach(bilibiliModel.availableCompatibleVariantPreferences, id: \.self) { preference in
                    Text(preference.title).tag(preference)
                }
            }
            .pickerStyle(.menu)

            Picker("Weak Network", selection: $bilibiliModel.playbackWeakNetworkPreference) {
                ForEach(bilibiliModel.availableWeakNetworkPreferences, id: \.self) { preference in
                    Text(preference.title).tag(preference)
                }
            }
            .pickerStyle(.menu)
        }
    }

    @ViewBuilder
    private var bilibiliTaskResults: some View {
        if !bilibiliModel.taskResults.isEmpty
            || bilibiliModel.taskResultsErrorMessage != nil
            || bilibiliModel.hasMoreTaskResults
        {
            if let summary = bilibiliModel.taskResultSummary {
                Text(taskResultsPaginationSummary(totalCount: summary.totalCount))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            if let errorMessage = bilibiliModel.taskResultsErrorMessage {
                Text(errorMessage)
                    .font(.caption)
                    .foregroundStyle(.red)
            }
            if bilibiliModel.taskResultsErrorMessage != nil && !bilibiliModel.hasMoreTaskResults {
                Button {
                    Task {
                        await bilibiliModel.retryTaskResults(serverAddressText: cacheModel.serverAddressText)
                    }
                } label: {
                    Label("Retry Results", systemImage: "arrow.clockwise")
                }
                .buttonStyle(.bordered)
                .disabled(bilibiliModel.isLoadingMoreTaskResults)
                .focused($focusedControl, equals: .bilibiliTaskResultsRetryButton)
            }
            if !bilibiliModel.taskResults.isEmpty {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 8) {
                        ForEach(bilibiliModel.taskResults) { result in
                            VStack(alignment: .leading, spacing: 6) {
                                HStack(alignment: .center, spacing: 10) {
                                    BilibiliTaskResultRow(result: result)

                                    Button {
                                        Task {
                                            await playBilibiliTaskResult(result)
                                        }
                                    } label: {
                                        Label("Play", systemImage: "play.fill")
                                    }
                                    .buttonStyle(.bordered)
                                    .disabled(!bilibiliModel.canPlay(result: result))
                                }

                                ForEach(result.artifacts) { artifact in
                                    BilibiliTaskArtifactRow(artifact: artifact)
                                }
                            }
                        }
                    }
                }
                .frame(maxHeight: 220)
            }

            if bilibiliModel.hasMoreTaskResults || bilibiliModel.isLoadingMoreTaskResults {
                Button {
                    Task {
                        await bilibiliModel.loadMoreTaskResults(
                            serverAddressText: cacheModel.serverAddressText
                        )
                    }
                } label: {
                    Label(
                        bilibiliModel.isLoadingMoreTaskResults ? "Loading Results" : "Load More Results",
                        systemImage: bilibiliModel.isLoadingMoreTaskResults
                            ? "hourglass"
                            : "chevron.down.circle"
                    )
                }
                .buttonStyle(.bordered)
                .disabled(!bilibiliModel.hasMoreTaskResults || bilibiliModel.isLoadingMoreTaskResults)
                .focused($focusedControl, equals: .bilibiliTaskResultsLoadMoreButton)
            }
        }
    }

    private var candidatePaginationSummary: String {
        let count = bilibiliModel.resolvedCandidates.count
        let noun = count == 1 ? "candidate" : "candidates"
        return bilibiliModel.hasMoreResolvedCandidates
            ? "\(count) \(noun) loaded | more available"
            : "\(count) \(noun) loaded | all available"
    }

    private func taskResultsPaginationSummary(totalCount: Int) -> String {
        let loadedCount = bilibiliModel.taskResults.count
        let noun = totalCount == 1 ? "result" : "results"
        if totalCount > loadedCount || bilibiliModel.hasMoreTaskResults {
            return "Showing \(loadedCount) of \(totalCount) \(noun) | next page available"
        }
        return "Showing \(loadedCount) of \(totalCount) \(noun)"
    }

    private var playbackArea: some View {
        VStack(alignment: .leading, spacing: 12) {
            playerSurface
            playbackControls
        }
    }

    private var playerSurface: some View {
        Group {
            if let player = model.player {
                VideoPlayer(player: player)
                    .id(model.loadedURL)
            } else {
                ZStack {
                    RoundedRectangle(cornerRadius: 8)
                        .fill(.white.opacity(0.08))
                    Text("Enter a local or remote stream URL")
                        .foregroundStyle(.secondary)
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var playbackControls: some View {
        HStack(spacing: 12) {
            Button {
                model.skipBackward()
            } label: {
                Label("10s", systemImage: "gobackward.10")
            }
            .disabled(!model.canUsePlaybackControls)

            Button {
                model.skipForward()
            } label: {
                Label("10s", systemImage: "goforward.10")
            }
            .disabled(!model.canUsePlaybackControls)

            Picker("Speed", selection: playbackSpeedBinding) {
                ForEach(PlayerPlaybackSpeed.allCases) { speed in
                    Text(speed.displayTitle).tag(speed)
                }
            }
            .pickerStyle(.segmented)
            .frame(width: 360)
        }
    }

    private var playbackSpeedBinding: Binding<PlayerPlaybackSpeed> {
        Binding(
            get: { model.playbackSpeed },
            set: { model.setPlaybackSpeed($0) }
        )
    }

    private func playCachedItem(_ item: CacheLibraryItem) async {
        let manualInteractionSequence = model.manualInteractionSequence
        guard let url = await cacheModel.playbackURL(for: item) else {
            return
        }

        let progressContext = playbackProgressContext(for: item, playbackURL: url)
        let didStartPlayback = model.loadTransient(
            streamURLText: url.absoluteString,
            progressContext: progressContext,
            ifManualInteractionSequenceMatches: manualInteractionSequence
        )
        bilibiliModel.clearPlaybackStatus()
        cacheModel.finishPreparedPlayback(for: item, didStartPlayback: didStartPlayback)
        if didStartPlayback {
            await refreshPlaybackProgressStatus()
        }
    }

    private func deleteCachedItem(_ item: CacheLibraryItem) async {
        let manualInteractionSequence = model.manualInteractionSequence
        let shouldStopActivePlayback =
            cacheModel.isActivePlaybackItem(item)
            || bilibiliModel.isActivePlaybackLibraryItem(id: item.id)
        let didRemove = await cacheModel.deleteItem(item) {
            if shouldStopActivePlayback, model.manualInteractionSequence == manualInteractionSequence {
                model.stop()
            }
        }
        if didRemove {
            bilibiliModel.clearTaskIfCachedLibraryItemDeleted(id: item.id)
        }
    }

    private func confirmDeleteCachedItem(_ item: CacheLibraryItem) {
        pendingDeleteItem = nil
        Task {
            await deleteCachedItem(item)
        }
    }

    @discardableResult
    private func selectDiscoveredServer(_ server: DiscoveredCacheServer) async -> CacheLibraryRefreshResult {
        discoveryModel.select(server)
        cacheModel.useDiscoveredServer(server)
        return await cacheModel.refresh()
    }

    private func autoConnectDiscoveredServerIfNeeded() async {
        guard
            !isAutoDiscoveryConnecting,
            !cacheModel.hasServerAddress,
            let server = discoveryModel.discoveredServers.first(where: {
                !failedAutoDiscoveryServerIDs.contains($0.id)
            })
        else {
            return
        }

        isAutoDiscoveryConnecting = true
        let refreshResult = await selectDiscoveredServer(server)
        isAutoDiscoveryConnecting = false
        switch refreshResult {
        case .succeeded:
            failedAutoDiscoveryServerIDs = []
        case .failed:
            markAutoDiscoveryFailure(server)
            await autoConnectDiscoveredServerIfNeeded()
        case .superseded:
            break
        }
    }

    private func markAutoDiscoveryFailure(_ server: DiscoveredCacheServer) {
        let inserted = failedAutoDiscoveryServerIDs.insert(server.id).inserted
        cacheModel.clearFailedDiscoveredServer(server)
        guard inserted else {
            return
        }

        Task { @MainActor in
            try? await Task.sleep(for: autoDiscoveryRetryDelay)
            failedAutoDiscoveryServerIDs.remove(server.id)
            await autoConnectDiscoveredServerIfNeeded()
        }
    }

    private func playBilibiliTask() async {
        let manualInteractionSequence = model.manualInteractionSequence
        guard let url = bilibiliModel.playableURL else {
            return
        }

        cacheModel.clearPlaybackStatus()
        let progressContext = bilibiliModel.playbackProgressContext(
            serverAddressText: cacheModel.serverAddressText
        )
        let didStartPlayback = model.loadTransient(
            streamURLText: url.absoluteString,
            progressContext: progressContext,
            ifManualInteractionSequenceMatches: manualInteractionSequence
        )
        bilibiliModel.finishPreparedPlayback(didStartPlayback: didStartPlayback)
        if didStartPlayback {
            await refreshPlaybackProgressStatus()
        }
    }

    private func playBilibiliTaskResult(_ result: BilibiliTaskResultPresentation) async {
        let manualInteractionSequence = model.manualInteractionSequence
        guard let url = bilibiliModel.playableURL(for: result) else {
            return
        }

        cacheModel.clearPlaybackStatus()
        let progressContext = bilibiliModel.playbackProgressContext(
            for: result,
            serverAddressText: cacheModel.serverAddressText
        )
        let didStartPlayback = model.loadTransient(
            streamURLText: url.absoluteString,
            progressContext: progressContext,
            ifManualInteractionSequenceMatches: manualInteractionSequence
        )
        bilibiliModel.finishPreparedPlayback(result: result, didStartPlayback: didStartPlayback)
        if didStartPlayback {
            await refreshPlaybackProgressStatus()
        }
    }

    private func loadManualStream() {
        cacheModel.clearPlaybackStatus()
        bilibiliModel.clearPlaybackStatus()
        model.load()
    }

    private func stopManualStream() {
        cacheModel.clearPlaybackStatus()
        bilibiliModel.clearPlaybackStatus()
        model.stop()
    }

    private func clearManualStream() {
        cacheModel.clearPlaybackStatus()
        bilibiliModel.clearPlaybackStatus()
        model.clear()
    }

    private func playbackProgressContext(
        for item: CacheLibraryItem,
        playbackURL: URL
    ) -> PlayerPlaybackProgressContext? {
        guard item.isOfflineHLSCache else {
            return nil
        }
        guard let endpoint = CacheServerEndpoint.normalized(from: cacheModel.serverAddressText) else {
            return nil
        }

        return PlayerPlaybackProgressContext(
            endpoint: endpoint,
            playbackURI: playbackURL.absoluteString,
            libraryItemID: item.id,
            variantID: item.primaryVariantID ?? ""
        )
    }

    private func refreshPlaybackProgressStatus() async {
        await model.flushPlaybackProgressReports()
        await cacheModel.refreshHLSCacheStatus()
    }
}

private struct BilibiliLoginQRCode: View {
    let payload: String
    private static let context = CIContext()

    var body: some View {
        Group {
            if let image = qrImage {
                Image(decorative: image, scale: 1)
                    .interpolation(.none)
                    .resizable()
                    .scaledToFit()
                    .padding(24)
                    .background(Color.white)
            } else {
                Image(systemName: "qrcode")
                    .resizable()
                    .scaledToFit()
                    .foregroundStyle(.secondary)
            }
        }
        .accessibilityHidden(true)
    }

    private var qrImage: CGImage? {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(payload.utf8)
        filter.correctionLevel = "M"
        guard let output = filter.outputImage?.transformed(by: CGAffineTransform(scaleX: 8, y: 8)) else {
            return nil
        }
        return Self.context.createCGImage(output, from: output.extent)
    }
}

private struct CacheLibraryRow: View {
    let item: CacheLibraryItem

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: item.availabilitySystemImage)
                .foregroundStyle(item.hasPlayableVariant ? Color.secondary : Color.red)
                .frame(width: 28)

            VStack(alignment: .leading, spacing: 8) {
                Text(item.displayTitle)
                    .font(.headline)
                    .lineLimit(2)
                if !item.subtitle.isEmpty {
                    Text(item.subtitle)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                HStack(spacing: 10) {
                    if let primaryVariant = item.primaryVariant {
                        Text(primaryVariant.displayLabel)
                    }
                    Text(item.availabilityLabel)
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.vertical, 8)
    }
}

private struct BilibiliTaskResultRow: View {
    let result: BilibiliTaskResultPresentation

    var body: some View {
        Label {
            VStack(alignment: .leading, spacing: 4) {
                Text(result.title)
                    .font(.callout.weight(.semibold))
                    .lineLimit(1)

                HStack(spacing: 8) {
                    Text(result.statusLabel)
                    if !result.subtitle.isEmpty {
                        Text(result.subtitle)
                    }
                    if !result.message.isEmpty, result.isFailed || result.isCancelled {
                        Text(result.message)
                    }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
            }
        } icon: {
            Image(systemName: result.statusSystemImage)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct BilibiliTaskArtifactRow: View {
    let artifact: BilibiliTaskArtifactPresentation

    private var detail: String {
        [artifact.kind, artifact.state, artifact.format, artifact.languageTag]
            .filter { !$0.isEmpty }
            .joined(separator: " | ")
    }

    var body: some View {
        Label {
            VStack(alignment: .leading, spacing: 2) {
                Text(artifact.title)
                    .font(.caption.weight(.semibold))
                    .lineLimit(1)
                if !detail.isEmpty {
                    Text(detail)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                if !artifact.message.isEmpty {
                    Text(artifact.message)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
            }
        } icon: {
            Image(systemName: artifact.isAvailable ? "doc.text" : "doc.text.magnifyingglass")
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.leading, 28)
    }
}

private struct BilibiliFetchNoticeRow: View {
    let notice: BilibiliFetchNotice

    private var color: Color {
        switch notice.tone {
        case .info:
            return .secondary
        case .warning:
            return .yellow
        case .error:
            return .red
        }
    }

    var body: some View {
        Label {
            VStack(alignment: .leading, spacing: 3) {
                Text(notice.title)
                    .font(.caption.weight(.semibold))
                Text(notice.message)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(3)
            }
        } icon: {
            Image(systemName: notice.systemImage)
        }
        .foregroundStyle(color)
    }
}

private struct CacheStatusBadgeRow: View {
    let badge: CacheStatusBadge

    private var color: Color {
        switch badge.tone {
        case .ready:
            return .green
        case .info:
            return .secondary
        case .warning:
            return .yellow
        case .error:
            return .red
        }
    }

    var body: some View {
        Label {
            VStack(alignment: .leading, spacing: 2) {
                Text(badge.label)
                    .font(.caption.weight(.semibold))
                if let detail = badge.detail, !detail.isEmpty {
                    Text(detail)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
            }
        } icon: {
            Image(systemName: badge.systemImage)
        }
        .foregroundStyle(color)
    }
}

private struct CacheRootRow: View {
    let root: CacheRoot

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: root.writable ? "externaldrive.fill" : "lock.fill")
            VStack(alignment: .leading, spacing: 2) {
                Text(root.displayLabel)
                    .font(.caption.weight(.semibold))
                Text(root.capacityLabel)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
        .foregroundStyle(.secondary)
    }
}

private struct ResolverSettingsSheet: View {
    @Environment(\.dismiss) private var dismiss
    @StateObject private var model: ResolverSettingsViewModel
    @State private var isEditorPresented = false
    @State private var editingEndpointID: String?
    @State private var showingDiscardConfirmation = false

    init(endpoint: CacheServerEndpoint) {
        _model = StateObject(
            wrappedValue: ResolverSettingsViewModel(client: GRPCCacheControlClient(endpoint: endpoint))
        )
    }

    var body: some View {
        NavigationStack {
            List {
                Section {
                    Label(ResolverSettingsViewModel.securityWarning, systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.yellow)
                        .fixedSize(horizontal: false, vertical: true)
                }

                if model.isLoading && model.snapshot == nil {
                    Section {
                        ProgressView("Loading resolver settings")
                    }
                }

                if !model.builtinEndpoints.isEmpty {
                    Section("Built-in Resolvers") {
                        ForEach(model.builtinEndpoints) { endpoint in
                            Toggle(
                                isOn: Binding(
                                    get: { !model.disabledBuiltinHostIDs.contains(endpoint.hostID) },
                                    set: { model.setBuiltin(endpoint, enabled: $0) }
                                )
                            ) {
                                VStack(alignment: .leading, spacing: 4) {
                                    Text(endpoint.name)
                                    Text(
                                        "\(endpoint.origin) · \(endpoint.regions.map(\.rawValue).joined(separator: ", "))"
                                    )
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                                }
                            }
                        }
                    }
                }

                Section("Custom Resolvers") {
                    ForEach(model.customEndpoints) { endpoint in
                        HStack(spacing: 16) {
                            Button {
                                editingEndpointID = endpoint.id
                                isEditorPresented = true
                            } label: {
                                VStack(alignment: .leading, spacing: 4) {
                                    Text(endpoint.name)
                                        .foregroundStyle(.primary)
                                    Text(
                                        "\(endpoint.origin) · \(endpoint.regions.map(\.rawValue).joined(separator: ", "))"
                                    )
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                                }
                            }
                            .buttonStyle(.plain)

                            Toggle(
                                "Enabled",
                                isOn: Binding(
                                    get: {
                                        model.customEndpoints.first(where: { $0.id == endpoint.id })?.enabled ?? false
                                    },
                                    set: { enabled in
                                        update(endpointID: endpoint.id, enabled: enabled)
                                    }
                                )
                            )

                            Button(role: .destructive) {
                                remove(endpointID: endpoint.id)
                            } label: {
                                Image(systemName: "trash")
                            }
                            .accessibilityLabel("Remove \(endpoint.name)")
                        }
                    }

                    Button {
                        editingEndpointID = nil
                        isEditorPresented = true
                    } label: {
                        Label("Add Resolver", systemImage: "plus")
                    }
                    .disabled(model.customEndpoints.count >= 32)
                }

                if let errorMessage = model.errorMessage {
                    Section {
                        Text(errorMessage)
                            .foregroundStyle(.red)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                } else if let statusMessage = model.statusMessage {
                    Section {
                        Text(statusMessage)
                            .foregroundStyle(.secondary)
                    }
                }
            }
            .navigationTitle("Resolver Settings")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Done") {
                        if model.hasChanges {
                            showingDiscardConfirmation = true
                        } else {
                            dismiss()
                        }
                    }
                    .accessibilityHint("Close resolver settings")
                }
                ToolbarItem(placement: .automatic) {
                    Button {
                        Task { await model.reload() }
                    } label: {
                        Label("Reload", systemImage: "arrow.clockwise")
                    }
                    .disabled(model.isLoading || model.isSaving)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button {
                        Task { await model.save() }
                    } label: {
                        Label(model.isSaving ? "Saving" : "Save", systemImage: "checkmark")
                    }
                    .disabled(!model.isAvailable || !model.hasChanges || model.isLoading || model.isSaving)
                }
            }
            .task { await model.load() }
            .confirmationDialog(
                "Discard Unsaved Changes?",
                isPresented: $showingDiscardConfirmation,
                titleVisibility: .visible
            ) {
                Button("Discard Changes", role: .destructive) { dismiss() }
                Button("Keep Editing", role: .cancel) {}
            } message: {
                Text("Your resolver changes have not been saved.")
            }
            .sheet(isPresented: $isEditorPresented) {
                ResolverCustomEndpointEditor(
                    endpoint: editingEndpointID.flatMap { id in model.customEndpoints.first { $0.id == id } }
                ) { name, origin, regions in
                    do {
                        if let editingEndpointID,
                            let index = model.customEndpoints.firstIndex(where: { $0.id == editingEndpointID })
                        {
                            try model.updateCustom(
                                at: index,
                                name: name,
                                origin: origin,
                                regions: regions,
                                enabled: model.customEndpoints[index].enabled
                            )
                        } else if editingEndpointID != nil {
                            return Self.message(for: .missingEntry)
                        } else {
                            try model.addCustom(name: name, origin: origin, regions: regions)
                        }
                        return nil
                    } catch let error as ResolverSettingsValidationError {
                        return Self.message(for: error)
                    } catch {
                        return error.localizedDescription
                    }
                }
            }
        }
    }

    private func update(endpointID: String, enabled: Bool) {
        guard let index = model.customEndpoints.firstIndex(where: { $0.id == endpointID }) else { return }
        let endpoint = model.customEndpoints[index]
        try? model.updateCustom(
            at: index,
            name: endpoint.name,
            origin: endpoint.origin,
            regions: endpoint.regions,
            enabled: enabled
        )
    }

    private func remove(endpointID: String) {
        guard let index = model.customEndpoints.firstIndex(where: { $0.id == endpointID }) else { return }
        model.removeCustom(at: index)
    }

    private static func message(for error: ResolverSettingsValidationError) -> String {
        switch error {
        case .invalidName: "Enter a name without control characters (128 bytes maximum)."
        case .invalidOrigin: "Enter an HTTPS origin with no path, credentials, query, or fragment."
        case .invalidRegions: "Select at least one region."
        case .duplicateHost: "That hostname is already used by another resolver."
        case .missingEntry: "This resolver is no longer available. Reload settings and try again."
        case .tooManyEntries: "A maximum of 32 custom resolvers is supported."
        }
    }
}

private struct ResolverCustomEndpointEditor: View {
    @Environment(\.dismiss) private var dismiss
    @State private var name: String
    @State private var origin: String
    @State private var selectedRegions: Set<ResolverRegion>
    @State private var validationMessage: String?

    let onSave: (String, String, [ResolverRegion]) -> String?

    init(
        endpoint: ResolverCustomEndpoint?,
        onSave: @escaping (String, String, [ResolverRegion]) -> String?
    ) {
        _name = State(initialValue: endpoint?.name ?? "")
        _origin = State(initialValue: endpoint?.origin ?? "https://")
        _selectedRegions = State(initialValue: Set(endpoint?.regions ?? [.all]))
        self.onSave = onSave
    }

    var body: some View {
        NavigationStack {
            Form {
                Section("Resolver") {
                    TextField("Name", text: $name)
                    TextField("HTTPS origin", text: $origin)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                }

                Section("Regions") {
                    ForEach(ResolverRegion.allCases) { region in
                        Toggle(
                            region.rawValue.uppercased(),
                            isOn: Binding(
                                get: { selectedRegions.contains(region) },
                                set: { isSelected in
                                    if isSelected {
                                        selectedRegions.insert(region)
                                    } else {
                                        selectedRegions.remove(region)
                                    }
                                }
                            )
                        )
                    }
                    ForEach(
                        selectedRegions.filter { !ResolverRegion.allCases.contains($0) }.sorted {
                            $0.rawValue < $1.rawValue
                        }
                    ) { region in
                        Label("Preserved region: \(region.rawValue)", systemImage: "lock")
                            .foregroundStyle(.secondary)
                    }
                }

                if let validationMessage {
                    Section {
                        Text(validationMessage)
                            .foregroundStyle(.red)
                    }
                }
            }
            .navigationTitle(name.isEmpty ? "Add Resolver" : "Edit Resolver")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        validationMessage = onSave(name, origin, selectedRegions.sorted { $0.rawValue < $1.rawValue })
                        if validationMessage == nil { dismiss() }
                    }
                    .disabled(name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || selectedRegions.isEmpty)
                }
            }
        }
    }
}
