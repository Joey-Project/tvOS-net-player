# Project TODO

- [pending] 在 Apple TV 实机上验证 `scripts/deploy-lan.sh` 的签名、安装和启动链路。
- [pending] 实现 LAN cache server MVP：gRPC 控制面、HTTP/HLS 媒体面、本地缓存目录扫描和播放 URL。
- [pending] 在 tvOS app 中接入 LAN cache server 控制面，先支持 server 配置、library list 和 playback source 获取。
- [pending] 接入 BBDown adapter：提交 Bilibili URL/BV 到 Mac mini cache server，下载完成后入库播放。
- [pending] 后续加入 Bonjour discovery、缓存淘汰和弱网/断网播放体验。
