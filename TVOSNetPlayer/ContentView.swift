import SwiftUI
import AVKit
import TVOSNetPlayerCore
import TVOSNetPlayerCacheClient

struct ContentView: View {
    @ObservedObject var model: PlayerViewModel
    @ObservedObject var cacheModel: CacheLibraryViewModel
    @ObservedObject var discoveryModel: CacheServerDiscoveryViewModel
    @ObservedObject var bilibiliModel: BilibiliTaskViewModel
    @State private var pendingDeleteItem: CacheLibraryItem?
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
                        playerSurface
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            .padding(.horizontal, 72)
            .padding(.vertical, 58)
        }
        .onAppear {
            discoveryModel.start()
            focusedControl =
                cacheModel.serverAddressText.isEmpty
                ? .cacheServerField
                : (model.streamURLText.isEmpty ? .refreshButton : .playButton)
            Task {
                await autoConnectDiscoveredServerIfNeeded()
            }
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
            Text("Delete \(item.displayTitle) from the LAN cache server.")
        }
    }

    private var cacheControls: some View {
        VStack(alignment: .leading, spacing: 20) {
            Text(cacheModel.serverName)
                .font(.title2.weight(.semibold))

            VStack(alignment: .leading, spacing: 10) {
                TextField("mac-mini.local:50051", text: $cacheModel.serverAddressText)
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
            }

            discoveryControls

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

            HStack(spacing: 10) {
                TextField("Quality", text: $bilibiliModel.qualityPreference)
                    .textContentType(.none)

                TextField("Codec", text: $bilibiliModel.encodingPreference)
                    .textContentType(.none)

                TextField("Audio", text: $bilibiliModel.audioLanguagePreference)
                    .textContentType(.none)
            }

            if let errorMessage = bilibiliModel.errorMessage {
                Text(errorMessage)
                    .font(.callout)
                    .foregroundStyle(.red)
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
            }

            if bilibiliModel.currentTask != nil || bilibiliModel.isSubmitting || bilibiliModel.isResolving {
                VStack(alignment: .leading, spacing: 8) {
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
                            .buttonStyle(.bordered)
                            .disabled(!bilibiliModel.canPlay(result: result))
                        }
                    }
                }
            }
            .frame(maxHeight: 220)
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

    private func playCachedItem(_ item: CacheLibraryItem) async {
        let manualInteractionSequence = model.manualInteractionSequence
        guard let url = await cacheModel.playbackURL(for: item) else {
            return
        }

        let didStartPlayback = model.loadTransient(
            streamURLText: url.absoluteString,
            ifManualInteractionSequenceMatches: manualInteractionSequence
        )
        bilibiliModel.clearPlaybackStatus()
        cacheModel.finishPreparedPlayback(for: item, didStartPlayback: didStartPlayback)
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
        let didStartPlayback = model.loadTransient(
            streamURLText: url.absoluteString,
            ifManualInteractionSequenceMatches: manualInteractionSequence
        )
        bilibiliModel.finishPreparedPlayback(didStartPlayback: didStartPlayback)
    }

    private func playBilibiliTaskResult(_ result: BilibiliTaskResultPresentation) async {
        let manualInteractionSequence = model.manualInteractionSequence
        guard let url = bilibiliModel.playableURL(for: result) else {
            return
        }

        cacheModel.clearPlaybackStatus()
        let didStartPlayback = model.loadTransient(
            streamURLText: url.absoluteString,
            ifManualInteractionSequenceMatches: manualInteractionSequence
        )
        bilibiliModel.finishPreparedPlayback(result: result, didStartPlayback: didStartPlayback)
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
