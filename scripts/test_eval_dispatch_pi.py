#!/usr/bin/env python3
"""Explicit optional Pi CLI interop test; requires a separately built pinned Pi."""
import asyncio
import json
import os
from pathlib import Path
import unittest

from test_eval_dispatch import GateTests


class PiInterop(GateTests):
    async def test_pi(self):
        cli = Path(os.environ['PI_EVAL_CLI']).resolve(strict=True)
        gate = await self.gate(); self.streaming = True
        workspace = self.root/'workspace'; workspace.mkdir()
        config = self.root/'pi-config'; config.mkdir(mode=0o700)
        settings = {'providers':{'openai':{'baseUrl':f'http://127.0.0.1:{gate.port}/v1',
                    'apiKey':gate.token,'modelOverrides':{'gpt-6-astra':{'maxTokens':4096}}}}}
        path = config/'models.json'; path.write_text(json.dumps(settings)); path.chmod(0o600)
        process = await asyncio.create_subprocess_exec('/usr/bin/node',str(cli),'-p',
            '--provider','openai','--model','gpt-6-astra','--thinking','medium','--no-session',
            '--no-extensions','--no-skills','--no-prompt-templates','do task',cwd=workspace,
            env={'PATH':'/usr/bin:/bin','HOME':str(self.root),'PI_CODING_AGENT_DIR':str(config)},
            stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.PIPE)
        try:
            out,err=await asyncio.wait_for(process.communicate(),15)
            self.assertEqual(process.returncode,0,err)
            self.assertIn(b'done',out)
            self.assertEqual(gate.summary()['dispatch_attempts'],1)
            self.assertEqual(gate.summary()['total_tokens'],5)
        finally:
            if process.returncode is None: process.kill()
            await process.communicate()


if __name__ == '__main__':
    unittest.main(defaultTest='PiInterop.test_pi')
