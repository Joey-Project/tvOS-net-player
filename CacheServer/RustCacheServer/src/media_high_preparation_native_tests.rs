#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the pinned muted macOS AVPlayer probe and local FFmpeg tools"]
async fn native_high_preparation_off_on_comparison() {
    crate::media_acceptance_tests::run_native_high_preparation_off_on_comparison().await;
}
