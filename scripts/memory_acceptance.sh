#!/bin/sh
# crab memory acceptance protocol
# 设计：docs/notes/2026-11-03-crab-memory-minimal-design.md §验收测试
# 本体自动化协议步骤 1/2/4/5/6（dsh 三步协议 + 三条质量杠）；
# 步骤 3（真实 LLM 真的用上召回的 lesson）只能人工，见文末。
#
# 用法：CRAB=/path/to/crab sh scripts/memory_acceptance.sh
set -eu

CRAB=${CRAB:-crab}
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
pass=0
fail=0

check() { # name condition...
  name=$1; shift
  if "$@" >/dev/null 2>&1; then
    echo "ok   — $name"; pass=$((pass+1))
  else
    echo "FAIL — $name"; fail=$((fail+1))
  fi
}

sess() { # dir text [supersedes] — 写一条 Lesson obs fixture
  dir=$1; text=$2; sup=${3:-null}
  mkdir -p "$dir/.crab/sessions"
  n=$(ls "$dir/.crab/sessions" 2>/dev/null | wc -l | tr -d ' ')
  f="$dir/.crab/sessions/session-$((n+1)).jsonl"
  printf '{"kind":"obs","id":1,"event":{"run_id":0,"ts":1,"obs":"lesson","kind":"fact","text":"%s","evidence":0,"supersedes":%s}}\n' \
    "$text" "$sup" > "$f"
}

echo "== memory acceptance: $CRAB =="

# 步骤 1+2：写入 → 同项目新会话召回（--show-system-prompt 即“模型看到的 prompt”）
A="$T/a"; mkdir -p "$A"
sess "$A" "in this project, sessions live under .crab/sessions"
out=$(cd "$A" && "$CRAB" --show-system-prompt)
check "2 recall: lesson in system prompt" sh -c "printf '%s' \"\$0\" | grep -q 'sessions live under .crab/sessions'" "$out"
check "2 recall: lessons section header"  sh -c "printf '%s' \"\$0\" | grep -q '# Lessons (from previous sessions in this project)'" "$out"

# 步骤 4：分层 — fact/procedure 分桶标注必须各自出现
sess "$A" "run cargo insta review after snapshot tests"
# procedure 桶：直接改 fixture 的 kind 字段
f=$(ls "$A"/.crab/sessions/session-*.jsonl | tail -1)
sed -i '' 's/"kind":"fact"/"kind":"procedure"/' "$f" 2>/dev/null || sed -i 's/"kind":"fact"/"kind":"procedure"/' "$f"
out=$(cd "$A" && "$CRAB" --show-system-prompt)
check "4 layering: [fact] bucket label"      sh -c "printf '%s' \"\$0\" | grep -q '\[fact\] in this project'" "$out"
check "4 layering: [procedure] bucket label" sh -c "printf '%s' \"\$0\" | grep -q '\[procedure\] run cargo insta review'" "$out"

# 步骤 5：纠正 — 同会话内 supersede 后旧 lesson 退出索引，新 lesson 存活
#（supersedes 是同会话 EntryId：写进 session-1.jsonl，id 指向该文件的旧 lesson）
printf '{"kind":"obs","id":2,"event":{"run_id":0,"ts":2,"obs":"lesson","kind":"fact","text":"NEWER: sessions live under .crab/sessions (v2)","evidence":0,"supersedes":1}}\n' \
  >> "$A/.crab/sessions/session-1.jsonl"
out=$(cd "$A" && "$CRAB" --show-system-prompt)
check "5 correction: superseding lesson wins" sh -c "printf '%s' \"\$0\" | grep -q 'sessions live under .crab/sessions (v2)'" "$out"
check "5 correction: retired lesson absent"   sh -c "! printf '%s' \"\$0\" | grep -q '\[fact\] in this project, sessions'" "$out"

# 步骤 6：隔离 — 项目 B 的 prompt 不得含项目 A 的 lesson
B="$T/b"; mkdir -p "$B"
outB=$(cd "$B" && "$CRAB" --show-system-prompt)
check "6 isolation: no lessons section in other project" sh -c "! printf '%s' \"\$0\" | grep -q '# Lessons'" "$outB"
check "6 isolation: other project prompt is base only"     sh -c "! printf '%s' \"\$0\" | grep -q 'sessions live under'" "$outB"

echo
echo "automated: $pass passed, $fail failed"
[ "$fail" -eq 0 ] || exit 1

cat <<'EOF'
步骤 3（人工，需要真实 provider）：
  1. cd 一个真实项目；CRAB=/path/to/crab 起 TUI，做一段真实工作
     （改几个文件、跑几次测试，留下可蒸馏的轨迹）。
  2. /reflect —— 逐条确认提议；notice 应报 "remembered N lesson(s)"。
  3. 退出；重新起 TUI（同项目，新会话）。
     $CRAB --show-system-prompt | grep Lessons  应看到刚确认的 lesson。
  4. 问一个只有该 lesson 能回答的问题（如"这个项目的会话存哪"），
     确认回答真的用上了召回内容，而不是重新探索。
EOF
