import SwiftUI
import AVKit
import TVOSNetPlayerCacheClient

struct ContentView: View {
    @ObservedObject var model: PlayerViewModel
    @ObservedObject var cacheModel: CacheLibraryViewModel
    @FocusState private var focusedControl: FocusedControl?

    private enum FocusedControl: Hashable {
        case cacheServerField
        case refreshButton
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
                    Text("\(model.statusMessage) \(cacheModel.statusMessage)")
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
            focusedControl =
                cacheModel.serverAddressText.isEmpty
                ? .cacheServerField
                : (model.streamURLText.isEmpty ? .refreshButton : .playButton)
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

            Divider()

            if cacheModel.items.isEmpty {
                ZStack {
                    RoundedRectangle(cornerRadius: 8)
                        .fill(.white.opacity(0.08))
                    Text("No cached videos")
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, minHeight: 220)
            } else {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 12) {
                        ForEach(cacheModel.items) { item in
                            Button {
                                Task {
                                    await playCachedItem(item)
                                }
                            } label: {
                                CacheLibraryRow(item: item)
                            }
                            .buttonStyle(.bordered)
                            .disabled(cacheModel.isLoading || !item.hasPlayableVariant)
                        }
                    }
                }
            }
        }
    }

    private var manualStreamControls: some View {
        VStack(alignment: .leading, spacing: 10) {
            TextField("http://192.168.1.10:8080/video.mp4", text: $model.streamURLText)
                .keyboardType(.URL)
                .submitLabel(.go)
                .onSubmit(model.load)
                .focused($focusedControl, equals: .urlField)

            if let validationMessage = model.validationMessage {
                Text(validationMessage)
                    .font(.callout)
                    .foregroundStyle(.red)
            }

            HStack(spacing: 18) {
                Button(action: model.load) {
                    Label("Play", systemImage: "play.fill")
                }
                .buttonStyle(.borderedProminent)
                .focused($focusedControl, equals: .playButton)

                Button(action: model.stop) {
                    Label("Stop", systemImage: "stop.fill")
                }
                .disabled(model.player == nil)

                Button(action: model.clear) {
                    Label("Clear", systemImage: "xmark.circle")
                }
                .disabled(!model.canClear)
            }
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
        guard let url = await cacheModel.playbackURL(for: item) else {
            return
        }

        model.load(streamURLText: url.absoluteString)
    }
}

private struct CacheLibraryRow: View {
    let item: CacheLibraryItem

    var body: some View {
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
                if let primaryVariant = item.variants.first {
                    Text(primaryVariant.displayLabel)
                }
                Text(item.source)
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.vertical, 8)
    }
}
