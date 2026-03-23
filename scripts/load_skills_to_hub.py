#!/usr/bin/env python3
"""
Batch-register all Claude Code skills into Sigil Hub global DB.

Reads SKILL.md files, extracts YAML frontmatter (name, description),
and registers each as a Hub capability via MCP protocol.
"""

import subprocess
import json
import sys
import os
import re
import time

SKILLS_DIR = os.path.expanduser("~/.claude/skills")
SERVER_BIN = os.path.join(os.path.dirname(os.path.dirname(__file__)), "target/release/memory-server")

# Use global DB only (no project context needed)
env = os.environ.copy()
env["ENABLE_PIPELINE"] = "false"
# Don't set MEMORY_DB_PATH — let it auto-resolve to ~/.sigil/global/memory.db

REQUEST_ID = 0

def next_id():
    global REQUEST_ID
    REQUEST_ID += 1
    return REQUEST_ID

def parse_frontmatter(content):
    """Extract YAML frontmatter from SKILL.md."""
    m = re.match(r'^---\s*\n(.*?)\n---\s*\n', content, re.DOTALL)
    if not m:
        return {}, content

    fm = {}
    for line in m.group(1).split('\n'):
        # Handle multiline description (indented continuation)
        if ':' in line and not line.startswith(' '):
            key, val = line.split(':', 1)
            fm[key.strip()] = val.strip()
        elif line.startswith(' ') and 'description' in fm:
            fm['description'] += ' ' + line.strip()

    body = content[m.end():]
    return fm, body

def send_request(proc, request):
    request_str = json.dumps(request) + "\n"
    proc.stdin.write(request_str.encode())
    proc.stdin.flush()
    response_line = proc.stdout.readline().decode().strip()
    if response_line:
        return json.loads(response_line)
    return None

def call_tool(proc, name, arguments):
    return send_request(proc, {
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments},
    })

def main():
    # Find all SKILL.md files (top-level only, skip nested .agents dirs)
    skills = []
    for entry in sorted(os.listdir(SKILLS_DIR)):
        skill_path = os.path.join(SKILLS_DIR, entry, "SKILL.md")
        if os.path.isfile(skill_path) and not entry.startswith('.'):
            skills.append((entry, skill_path))

    print(f"Found {len(skills)} skills in {SKILLS_DIR}")

    # Start server
    print("Starting memory-server...")
    proc = subprocess.Popen(
        [SERVER_BIN],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )

    try:
        # Initialize
        send_request(proc, {
            "jsonrpc": "2.0", "id": next_id(), "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "skill-loader", "version": "1.0.0"},
            },
        })
        proc.stdin.write(json.dumps(
            {"jsonrpc": "2.0", "method": "notifications/initialized"}
        ).encode() + b"\n")
        proc.stdin.flush()

        registered = 0
        errors = 0

        for dirname, skill_path in skills:
            with open(skill_path, 'r') as f:
                content = f.read()

            fm, body = parse_frontmatter(content)
            name = fm.get('name', dirname)
            description = fm.get('description', '')

            # Truncate description for the summary field
            short_desc = description[:200] if description else f"Skill: {name}"

            skill_id = f"skill:{name}"

            # The definition contains the full SKILL.md content
            definition = json.dumps({
                "format": "claude-code-skill-markdown",
                "source_path": skill_path,
                "content": content,
            })

            resp = call_tool(proc, "hub_register", {
                "id": skill_id,
                "cap_type": "skill",
                "name": name,
                "description": short_desc,
                "definition": definition,
                "scope": "global",
                "version": 1,
            })

            # Check response
            try:
                result = resp["result"]["content"][0]["text"]
                data = json.loads(result)
                if "error" in data:
                    print(f"  ERR   {skill_id}: {data['error']}")
                    errors += 1
                else:
                    print(f"  OK    {skill_id}")
                    registered += 1
            except (KeyError, TypeError, json.JSONDecodeError) as e:
                print(f"  ERR   {skill_id}: {e}")
                errors += 1

        # Show stats
        print(f"\n{'='*40}")
        print(f"Registered: {registered}")
        print(f"Errors: {errors}")

        # Verify with hub_stats
        resp = call_tool(proc, "hub_stats", {})
        try:
            result = resp["result"]["content"][0]["text"]
            stats = json.loads(result)
            print(f"Hub total: {stats.get('total_capabilities', '?')}")
            print(f"By type: {stats.get('by_type', '?')}")
        except:
            pass

        return errors == 0

    finally:
        proc.terminate()
        proc.wait()
        stderr = proc.stderr.read().decode()
        # Only print stderr if there were issues
        if errors > 0 and stderr:
            print(f"\n--- Server stderr ---\n{stderr[-500:]}")

if __name__ == "__main__":
    success = main()
    sys.exit(0 if success else 1)
