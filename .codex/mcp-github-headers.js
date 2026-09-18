#!/usr/bin/env node
// Prints the Authorization header for the GitHub MCP server.
//
// The token is read from the git credential store at connection time, so no
// secret is written into .codex/config.toml. Codex refreshes this helper after
// a 401/403 when its output changes, so a rotated token can be picked up.

'use strict';

const { execFileSync } = require('child_process');

const GIT_CANDIDATES = [
  'git',
  'C:/Program Files/Git/cmd/git.exe',
  'C:/Program Files (x86)/Git/cmd/git.exe',
  '/mingw64/bin/git',
];

function readToken() {
  for (const git of GIT_CANDIDATES) {
    try {
      const out = execFileSync(git, ['credential', 'fill'], {
        input: 'protocol=https\nhost=github.com\n\n',
        encoding: 'utf8',
        stdio: ['pipe', 'pipe', 'ignore'],
        timeout: 5000,
        windowsHide: true,
        env: { ...process.env, GIT_TERMINAL_PROMPT: '0' },
      });
      const match = out.match(/^password=(.*)$/m);
      if (match && match[1].trim()) return match[1].trim();
    } catch {
      // Try the next candidate location.
    }
  }
  return null;
}

const token = readToken();

// An empty object fails closed: Codex connects without an Authorization header,
// gets a 401, and reports the server as unauthenticated rather than sending a
// malformed credential.
process.stdout.write(
  token ? JSON.stringify({ Authorization: `Bearer ${token}` }) : '{}'
);
