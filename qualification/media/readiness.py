"""Observe the same published HLS buffer for gateway and raw FFmpeg trials."""

from pathlib import Path
import re


def published_buffer(path, live):
    path = Path(path)
    try:
        text = path.read_text()
        durations = [
            float(line.split(":", 1)[1].split(",")[0])
            for line in text.splitlines()
            if line.startswith("#EXTINF:")
        ]
        if not durations or any(value <= 0 for value in durations):
            return None
        target = max(
            2,
            next(
                (
                    float(line.split(":", 1)[1])
                    for line in text.splitlines()
                    if line.startswith("#EXT-X-TARGETDURATION:")
                ),
                2,
            ),
        )
        if "#EXT-X-ENDLIST" not in text and sum(durations) + 1e-6 < target * (
            2 if live else 1
        ):
            return None
        files = [
            line for line in text.splitlines() if line and not line.startswith("#")
        ]
        files += re.findall(r'^#EXT-X-MAP:URI="([^"]+)"', text, re.MULTILINE)
        if len(
            [line for line in text.splitlines() if line and not line.startswith("#")]
        ) != len(durations):
            return None
        if any(
            Path(name).name != name
            or not (path.parent / name).is_file()
            or not (path.parent / name).stat().st_size
            for name in files
        ):
            return None
        return sum(durations)
    except (OSError, ValueError, UnicodeError):
        return None
