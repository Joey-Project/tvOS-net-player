import SwiftUI
import AVKit

struct ContentView: View {
    @ObservedObject var model: PlayerViewModel

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

                HStack(spacing: 18) {
                    TextField("http://192.168.1.10:8080/video.mp4", text: $model.streamURLText)
                        .keyboardType(.URL)
                        .submitLabel(.go)
                        .onSubmit(model.load)

                    Button("Play", action: model.load)
                        .buttonStyle(.borderedProminent)
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
        .onAppear(perform: model.loadDefaultIfAvailable)
    }
}
