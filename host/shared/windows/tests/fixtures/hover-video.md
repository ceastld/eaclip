# Synthetic hover video fixture

`hover-video.mp4` is a generated 1-second 160×90, 10 fps H.264 test pattern with
an AAC sine tone. It contains no user content. Native preview selects only the
video stream; the tone allows this fixture to include an otherwise valid audio
track without connecting an audio sink.

Generated from the repository root:

```powershell
ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=size=160x90:rate=10 -f lavfi -i sine=frequency=440:sample_rate=44100 -t 1 -c:v libx264 -pix_fmt yuv420p -preset veryfast -c:a aac -movflags +faststart -y host/shared/windows/tests/fixtures/hover-video.mp4
```

Used by the native popup regression and the frontend `view=file-preview`
design stage. Tests copy the fixture into an isolated repository `.tmp/` root.
