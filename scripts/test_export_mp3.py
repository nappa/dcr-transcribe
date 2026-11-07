import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import wave

from export_mp3 import export, sha256


@unittest.skipUnless(shutil.which('ffmpeg') and shutil.which('ffprobe'), 'ffmpeg/ffprobe required')
class ExportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def source(self, name, channel=0, finalized=True):
        path = self.root / name
        with wave.open(str(path), 'wb') as audio:
            audio.setnchannels(1)
            audio.setsampwidth(2)
            audio.setframerate(16000)
            audio.writeframes(b'\x01\x00' * 16000)
        events = [dict(type='header', version=1, sample_rate=16000, channel=channel)]
        if finalized:
            events.append(dict(type='finalized', samples=16000))
        path.with_suffix('.timing.jsonl').write_text('\n'.join(map(json.dumps, events)) + '\n')
        return path

    def test_combination_hashes_positions_and_no_overwrite(self):
        # 引用符や空白のある元パスも concat 文法へ直接埋め込まない。
        inputs = [self.source("part 'one.wav"), self.source('part two.wav')]
        output = self.root / 'review'
        export(inputs, output)
        manifest = json.loads((output / 'timeline.json').read_text())
        self.assertEqual([r['mp3_pcm_start_sample'] for r in manifest['sources']], [0, 16000])
        self.assertEqual(manifest['sources'][0]['sha256'], sha256(inputs[0]))
        self.assertEqual(manifest['mp3_sha256'], sha256(output / 'review.mp3'))
        probe = subprocess.check_output(['ffprobe', '-v', 'error', '-show_entries',
                                         'stream=codec_name,channels:format=duration', '-of', 'json',
                                         str(output / 'review.mp3')])
        info = json.loads(probe)
        self.assertEqual(info['streams'][0]['codec_name'], 'mp3')
        self.assertEqual(info['streams'][0]['channels'], 1)
        self.assertAlmostEqual(float(info['format']['duration']), 2, delta=0.15)
        with self.assertRaises(ValueError):
            export(inputs, output)

    def test_rejects_unfinished_and_mixed_channels(self):
        with self.assertRaises(ValueError):
            export([self.source('unfinished.wav', finalized=False)], self.root / 'unfinished')
        with self.assertRaises(ValueError):
            export([self.source('zero.wav'), self.source('one.wav', channel=1)], self.root / 'mixed')
        self.assertFalse((self.root / 'mixed').exists())

    def test_failed_encoder_leaves_no_bundle(self):
        from unittest.mock import patch
        source = self.source('input.wav')
        output = self.root / 'failure'
        with patch('export_mp3.subprocess.run', side_effect=subprocess.CalledProcessError(1, 'ffmpeg')):
            with self.assertRaises(subprocess.CalledProcessError):
                export([source], output)
        self.assertFalse(output.exists())


if __name__ == '__main__':
    unittest.main()
