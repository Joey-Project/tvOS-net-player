import SwiftUI
import AVKit

struct ContentView: View {
    @ObservedObject var model: PlayerViewModel
    @FocusState private var focusedControl: FocusedControl?

    private enum FocusedControl: Hashable {
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
                    Text(model.statusMessage)
                        .foregroundStyle(.secondary)
                }

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
            .padding(.horizontal, 86)
            .padding(.vertical, 64)
        }
        .onAppear {
            focusedControl = model.streamURLText.isEmpty ? .urlField : .playButton
        }
    }
}
