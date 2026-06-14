# Project TODO

- [pending] 在 Apple TV 实机上验证 `scripts/deploy-lan.sh` 的签名、安装和启动链路。
- [pending] 为 LAN cache library 增加完整分页/search UI，替代当前最多 200 条的首屏 preview。
- [pending] 为持久化 task state 增加 retention/cleanup 策略，避免历史任务无限增长。
- [pending] 在 tvOS app 增加 Bilibili URL/BV task 提交和进度 UI。
- [pending] 扩展 Bilibili task options/result schema，支持显式 page/episode/all selection 或多 item 结果。
- [in_progress] 分四个 PR 落地 HLS progressive cache：BBDown 0.2.0 playback planning、progressive control plane、HLS media pipeline、offline cache finalization/recovery。
- [pending] 后续加入 Bonjour discovery、缓存淘汰和更完整的弱网/断网播放体验。
