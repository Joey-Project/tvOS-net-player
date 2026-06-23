import AVKit
import SwiftUI
import TVOSNetPlayerCacheClient
import TVOSNetPlayerCore

struct ContentView: View {
    @ObservedObject var model: PlayerViewModel
    @ObservedObject var cacheModel: CacheLibraryViewModel
    @ObservedObject var discoveryModel: CacheServerDiscoveryViewModel
    @ObservedObject var bilibiliModel: BilibiliTaskViewModel
    @State private var selectedItemID: CacheLibraryItem.ID?
    @State private var pendingDeleteItem: CacheLibraryItem?
    @State private var isAutoDiscoveryConnecting = false
    @State private var failedAutoDiscoveryServerIDs: Set<String> = []
    private let autoDiscoveryRetryDelay: Duration = .seconds(30)

    var body: some View {
        NavigationSplitView {
            cacheSidebar
        } detail: {
            detailPane
        }
        .frame(minWidth: 1100, minHeight: 720)
        .onAppear(perform: selectFirstCacheItemIfNeeded)
        .onAppear {
            discoveryModel.start()
            Task {
                await autoConnectDiscoveredServerIfNeeded()
            }
        }
        .onChange(of: cacheModel.items) { _, _ in
            selectFirstCacheItemIfNeeded()
        }
        .onChange(of: discoveryModel.discoveredServers) { _, _ in
            Task {
                await autoConnectDiscoveredServerIfNeeded()
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
    }

    private var selectedItem: CacheLibraryItem? {
        if let selectedItemID,
            let item = cacheModel.items.first(where: { $0.id == selectedItemID })
        {
            return item
        }

        return cacheModel.items.first
    }

    private var cacheSidebar: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 8) {
                TextField(
                    "mac-mini.local:50051 or https://cache.example.com",
                    text: $cacheModel.serverAddressText
                )
                .textFieldStyle(.roundedBorder)
                .onSubmit {
                    Task {
                        await cacheModel.refresh()
                    }
                }

                Button {
                    Task {
                        await cacheModel.refresh()
                    }
                } label: {
                    Label(cacheModel.isLoading ? "Loading" : "Refresh", systemImage: "arrow.clockwise")
                }
                .disabled(!cacheModel.canRefresh)
            }

            discoveryControls

            HStack(spacing: 8) {
                TextField("Search cached videos", text: $cacheModel.searchText)
                    .textFieldStyle(.roundedBorder)
                    .onSubmit {
                        Task {
                            await cacheModel.refresh()
                        }
                    }

                Button {
                    Task {
                        await cacheModel.refresh()
                    }
                } label: {
                    Label(cacheModel.hasPendingSearch ? "Search" : "Reload", systemImage: "magnifyingglass")
                }
                .disabled(!cacheModel.canRefresh)
            }

            if let errorMessage = cacheModel.errorMessage {
                Text(errorMessage)
                    .font(.callout)
                    .foregroundStyle(.red)
                    .lineLimit(3)
            }

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

            Divider()

            if cacheModel.items.isEmpty {
                VStack(spacing: 8) {
                    ContentUnavailableView("No Cached Videos", systemImage: "externaldrive")
                        .frame(maxWidth: .infinity, maxHeight: .infinity)

                    cacheLoadMoreButton
                }
            } else {
                VStack(spacing: 8) {
                    List(selection: $selectedItemID) {
                        ForEach(cacheModel.items) { item in
                            CacheLibraryRow(item: item)
                                .tag(item.id)
                        }
                    }

                    cacheLoadMoreButton
                }
            }
        }
        .padding(16)
        .navigationTitle(cacheModel.serverName)
    }

    private var discoveryControls: some View {
        HStack(spacing: 8) {
            Menu {
                ForEach(discoveryModel.discoveredServers) { server in
                    Button {
                        Task {
                            await selectDiscoveredServer(server)
                        }
                    } label: {
                        Text(server.displayName)
                    }
                }
            } label: {
                Label("LAN Servers", systemImage: "network")
            }
            .disabled(discoveryModel.discoveredServers.isEmpty || cacheModel.isLoading)

            Text(discoveryModel.statusMessage)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
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
            .disabled(!cacheModel.canLoadMore)
        }
    }

    private var detailPane: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack(alignment: .firstTextBaseline, spacing: 12) {
                Text("macOS Net Player")
                    .font(.title2.weight(.semibold))

                if cacheModel.isLoading {
                    ProgressView()
                        .controlSize(.small)
                }

                Spacer()

                Text("\(model.statusMessage) \(cacheModel.statusMessage) \(bilibiliModel.statusMessage)")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                    .multilineTextAlignment(.trailing)
            }

            manualStreamControls
            bilibiliControls
            selectedCacheItemControls
            playbackArea
        }
        .padding(24)
    }

    private var manualStreamControls: some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 10) {
                TextField("http://192.168.1.10:8080/video.mp4", text: $model.streamURLText)
                    .textFieldStyle(.roundedBorder)
                    .onSubmit(loadManualStream)

                if let validationMessage = model.validationMessage {
                    Text(validationMessage)
                        .font(.callout)
                        .foregroundStyle(.red)
                }

                HStack(spacing: 10) {
                    Button(action: loadManualStream) {
                        Label("Play", systemImage: "play.fill")
                    }
                    .buttonStyle(.borderedProminent)

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
            .padding(.top, 4)
        } label: {
            Label("Stream URL", systemImage: "link")
        }
    }

    private var bilibiliControls: some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 12) {
                TextField("BV1xx411c7mD or Bilibili URL", text: $bilibiliModel.sourceText)
                    .textFieldStyle(.roundedBorder)
                    .onSubmit {
                        Task {
                            await bilibiliModel.submit(serverAddressText: cacheModel.serverAddressText)
                        }
                    }

                HStack(spacing: 10) {
                    TextField("Quality", text: $bilibiliModel.qualityPreference)
                        .textFieldStyle(.roundedBorder)
                        .frame(width: 110)

                    TextField("Codec", text: $bilibiliModel.encodingPreference)
                        .textFieldStyle(.roundedBorder)
                        .frame(width: 110)

                    TextField("Audio", text: $bilibiliModel.audioLanguagePreference)
                        .textFieldStyle(.roundedBorder)
                        .frame(width: 110)

                    Spacer()
                }

                if let errorMessage = bilibiliModel.errorMessage {
                    Text(errorMessage)
                        .font(.callout)
                        .foregroundStyle(.red)
                        .lineLimit(3)
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
                            .frame(width: 180)

                            Picker("To", selection: $bilibiliModel.rangeEndCandidateID) {
                                ForEach(bilibiliModel.resolvedCandidates) { candidate in
                                    Text(candidate.title).tag(Optional(candidate.selectionID))
                                }
                            }
                            .frame(width: 180)
                        }
                    }

                    if let selectionSummary = bilibiliModel.candidateSelectionSummary {
                        Text(selectionSummary)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                    }

                    HStack(spacing: 10) {
                        bilibiliReResolveButton

                        Button {
                            bilibiliModel.clearResolvedCandidateSelection()
                        } label: {
                            Label("Clear Selection", systemImage: "xmark.circle")
                        }
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
                                        Spacer()
                                    }
                                }
                                .buttonStyle(.bordered)
                                .disabled(bilibiliModel.candidateSelectionMode == .all)
                            }
                        }
                    }
                    .frame(maxHeight: 220)
                }

                if bilibiliModel.currentTask != nil || bilibiliModel.isSubmitting || bilibiliModel.isResolving {
                    VStack(alignment: .leading, spacing: 6) {
                        ProgressView(value: bilibiliModel.progress)
                        Text(bilibiliModel.statusMessage)
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                        if let badge = bilibiliModel.progressiveCacheStatusBadge {
                            Label(badge.label, systemImage: badge.systemImage)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                        bilibiliTaskResults
                    }
                }

                HStack(spacing: 10) {
                    Button {
                        Task {
                            await bilibiliModel.submit(serverAddressText: cacheModel.serverAddressText)
                        }
                    } label: {
                        Label(bilibiliModel.submitButtonTitle, systemImage: "plus.circle.fill")
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(!bilibiliModel.canSubmit)

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
            .padding(.top, 4)
        } label: {
            Label("Bilibili", systemImage: "play.tv")
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
        .disabled(!bilibiliModel.canReResolve)
    }

    private var selectedCacheItemControls: some View {
        GroupBox {
            HStack(alignment: .top, spacing: 16) {
                if let selectedItem {
                    CacheLibraryMetadata(item: selectedItem)

                    Spacer(minLength: 16)

                    Button {
                        Task {
                            await playCachedItem(selectedItem)
                        }
                    } label: {
                        Label("Play Cached", systemImage: "play.rectangle.fill")
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(
                        cacheModel.isLoading
                            || cacheModel.deletingItemIDs.contains(selectedItem.id)
                            || !selectedItem.hasPlayableVariant
                    )

                    Button {
                        pendingDeleteItem = selectedItem
                    } label: {
                        Label(
                            cacheModel.deletingItemIDs.contains(selectedItem.id) ? "Deleting" : "Delete",
                            systemImage: "trash"
                        )
                    }
                    .disabled(!cacheModel.canDelete(selectedItem))
                } else {
                    Text("No cached video selected")
                        .foregroundStyle(.secondary)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.top, 4)
        } label: {
            Label("Cached Video", systemImage: "externaldrive.fill")
        }
    }

    @ViewBuilder
    private var bilibiliTaskResults: some View {
        if !bilibiliModel.taskResults.isEmpty {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 8) {
                    ForEach(bilibiliModel.taskResults) { result in
                        HStack(alignment: .center, spacing: 10) {
                            BilibiliTaskResultRow(result: result)

                            Button {
                                Task {
                                    await playBilibiliTaskResult(result)
                                }
                            } label: {
                                Label("Play", systemImage: "play.fill")
                            }
                            .disabled(!bilibiliModel.canPlay(result: result))
                        }
                    }
                }
            }
            .frame(maxHeight: 180)
        }
    }

    private var playbackArea: some View {
        VStack(alignment: .leading, spacing: 10) {
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
                ContentUnavailableView("No Stream Loaded", systemImage: "play.rectangle")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(.black.opacity(0.08))
        .clipShape(RoundedRectangle(cornerRadius: 8))
    }

    private var playbackControls: some View {
        HStack(spacing: 10) {
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
            .frame(width: 280)
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
        Task {
            await refreshPlaybackProgressStatus()
        }
    }

    private func clearManualStream() {
        cacheModel.clearPlaybackStatus()
        bilibiliModel.clearPlaybackStatus()
        model.clear()
    }

    private func selectFirstCacheItemIfNeeded() {
        guard !cacheModel.items.isEmpty else {
            selectedItemID = nil
            return
        }

        if let selectedItemID,
            cacheModel.items.contains(where: { $0.id == selectedItemID })
        {
            return
        }

        selectedItemID = cacheModel.items.first?.id
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

private struct CacheLibraryRow: View {
    let item: CacheLibraryItem

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: item.availabilitySystemImage)
                .foregroundStyle(item.hasPlayableVariant ? Color.secondary : Color.red)

            VStack(alignment: .leading, spacing: 5) {
                Text(item.displayTitle)
                    .font(.headline)
                    .lineLimit(2)

                if !item.subtitle.isEmpty {
                    Text(item.subtitle)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }

                HStack(spacing: 8) {
                    if let primaryVariant = item.primaryVariant {
                        Text(primaryVariant.displayLabel)
                    }
                    Text(item.availabilityLabel)
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 4)
    }
}

private struct CacheLibraryMetadata: View {
    let item: CacheLibraryItem

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(item.displayTitle)
                .font(.headline)
                .lineLimit(2)

            if !item.subtitle.isEmpty {
                Text(item.subtitle)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }

            HStack(spacing: 12) {
                Label(item.availabilityLabel, systemImage: item.availabilitySystemImage)

                if let primaryVariant = item.primaryVariant {
                    Text(primaryVariant.displayLabel)
                }
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
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

private struct BilibiliFetchNoticeRow: View {
    let notice: BilibiliFetchNotice

    private var color: Color {
        switch notice.tone {
        case .info:
            return .secondary
        case .warning:
            return .orange
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
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(3)
            }
        } icon: {
            Image(systemName: notice.systemImage)
        }
        .foregroundStyle(color)
    }
}

private struct CacheRootRow: View {
    let root: CacheRoot

    var body: some View {
        Label {
            VStack(alignment: .leading, spacing: 2) {
                Text(root.displayLabel)
                    .font(.caption.weight(.semibold))
                Text(root.capacityLabel)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        } icon: {
            Image(systemName: root.writable ? "externaldrive.fill" : "lock.fill")
        }
        .foregroundStyle(.secondary)
    }
}
