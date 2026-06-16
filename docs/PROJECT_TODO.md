# Project TODO

- [completed] 在共享 AppCore、tvOS app 和 macOS app 增加 Bilibili URL/BV task 提交和进度 UI。
- [pending] 在 Apple TV 实机上验证 `scripts/deploy-lan.sh` 的签名、安装和启动链路。
- [completed] 为 LAN cache library 增加完整分页/search UI，替代当前最多 200 条的首屏 preview。
- [completed] 为持久化 task state 增加 retention/cleanup 策略，避免历史任务无限增长。
- [completed] 增加 cache root 展示、可见缓存项删除和离线 HLS 状态 UX。
- [completed] PR A：加入 Bonjour discovery、自动连接、server picker 和 manual fallback。
- [pending] PR B：加入 HLS cache 50 GiB 默认 quota、90% high watermark、80% low watermark 和自动淘汰。
- [pending] PR C：加入弱网 progressive fill scheduler、旧播放 demotion/FILO、first-frame prewarm 和更完整状态 UX。
- [pending] PR D：加入 Bilibili resolve/select multi-result control-plane schema 和 tvOS/macOS selection UI。
