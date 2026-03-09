# Native Audio Support Plugin (原生音频支持插件)

此插件为 **Ting Reader** 提供广泛的原生音频格式支持，包括 **M4A, WMA, FLAC, APE, WAV, OGG, OPUS, AAC** 等。它提供元数据提取（标题、艺术家、专辑、封面等）和流式播放（实时转码为 MP3）功能。

## 功能特性

- **多格式支持**：支持 M4A, WMA, FLAC, APE, WAV, OGG, OPUS, AAC 等多种常见音频格式。
- **元数据提取**：使用 `ffprobe` 深度读取音频文件的标签信息和内嵌封面。
- **流式播放**：通过 `ffmpeg` 实时将非浏览器原生支持的格式（如 WMA, FLAC, APE）转码为 MP3 流，实现 Web 端无缝播放。
- **智能依赖检测**：自动从多个来源（配置、内置、共享插件或系统路径）查找 FFmpeg。

## 依赖说明

此插件需要 **FFmpeg** 才能正常工作。它将按照以下顺序查找 FFmpeg 可执行文件：

1.  **配置路径**：在插件设置界面中手动指定的路径。
2.  **内置文件**：如果将 `ffmpeg` 和 `ffprobe` 直接放入此插件文件夹中。
3.  **共享插件**：如果已安装 `ffmpeg-utils` 插件（推荐方式）。
4.  **系统路径**：如果操作系统已全局安装 `ffmpeg` (PATH 环境变量)。

**推荐做法**：同时安装 `ffmpeg-utils` 插件，以获得最佳兼容性。

## 安装说明

1.  下载最新发行版。
2.  将 `native-audio-support` 文件夹解压到您的 Ting Reader `plugins` 目录下。
3.  重启 Ting Reader。

## 配置说明

如果自动检测失败，您可以在 Ting Reader 的插件设置中手动指定 FFmpeg 的路径。

## 源码构建

```bash
cargo build --release
```

## 许可证

MIT License. 详见 [LICENSE](LICENSE) 文件。
