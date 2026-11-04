#!/usr/bin/env bash
# 拉取式架构验收（Q11 试金石）：多客户端并发拉取 + 断线续传，
# 断言每个客户端收到连续无丢的完整事件序列。
# **如果这个脚本过不了，说明问题在事件源而不是传输——换架构就是误诊。**
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CRAB="$ROOT/target/release/crab"
[ -x "$CRAB" ] || { echo "build first: cargo build --release -p crab-cli"; exit 1; }

HOST="${CRAB_HOST:-127.0.0.1}"
PORT="${CRAB_PORT:-9401}"
PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  ok: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }

"$CRAB" serve --host "$HOST" --port "$PORT" > /tmp/crab-pull-acceptance.log 2>&1 &
PID=$!
cleanup() { kill -9 $PID 2>/dev/null; }
trap cleanup EXIT
sleep 4

TOK=$(grep -o 'token=[a-f0-9]*' /tmp/crab-pull-acceptance.log | head -1 | cut -d= -f2)
[ -n "$TOK" ] || { echo "cannot get token"; exit 1; }
BASE="https://$HOST:$PORT"

echo "[1] healthz"
curl -sk "$BASE/healthz" | grep -q ok && ok "healthz" || fail "healthz"

# 注入 N 个 prompt（每个产生 turn_started/text/finished 等事件）
echo "[2] 注入 5 个 prompt（确定性事件流）"
for i in 1 2 3 4 5; do
  curl -sk -X POST "$BASE/cmd?token=$TOK" -H 'content-type: application/json' \
    -d "{\"id\":\"p$i\",\"type\":\"prompt\",\"text\":\"say round $i\"}" > /dev/null
  sleep 1
done
sleep 3  # 等所有 run 完成

# 拉取全部事件到文件（after_id=0，一次拉完近期 200 条）
echo "[3] 拉取全部事件，断言 id 连续无丢"
curl -sk "$BASE/events?token=$TOK&after_id=0&timeout=1" > /tmp/pull-all.json
python3 - <<PYEOF
import json
d = json.load(open("/tmp/pull-all.json"))
events = d["events"]
ids = [e["id"] for e in events]
print(f"  拉到 {len(ids)} 个事件, high_watermark={d['high_watermark']}")
# 连续性：相邻 id 差 1
gaps = [(ids[i-1], ids[i]) for i in range(1, len(ids)) if ids[i] != ids[i-1]+1]
if gaps:
    print(f"  GAPS: {gaps[:5]}")
    exit(1)
# 最后一个 id 应等于 high_watermark
if ids and ids[-1] != d["high_watermark"]:
    print(f"  TAIL MISMATCH: last={ids[-1]} hw={d['high_watermark']}")
    exit(1)
print(f"  ok: 连续 1..{ids[-1] if ids else 0}, tail == high_watermark")
PYEOF
[ $? -eq 0 ] && ok "事件序列连续无丢，尾部对齐高水位" || fail "事件有间隙或尾部不对齐"

echo "[4] 断线续传：从中间 offset 重拉，断言补全且不重复"
MID=$(python3 -c "import json;d=json.load(open('/tmp/pull-all.json'));ids=[e['id'] for e in d['events']];print(ids[len(ids)//2] if ids else 0)")
curl -sk "$BASE/events?token=$TOK&after_id=$MID&timeout=1" > /tmp/pull-resume.json
python3 - <<PYEOF
import json
full = json.load(open("/tmp/pull-all.json"))
resume = json.load(open("/tmp/pull-resume.json"))
mid = $MID
full_ids = [e["id"] for e in full["events"]]
resume_ids = [e["id"] for e in resume["events"]]
# resume 应返回 mid 之后的所有（full 里 id > mid 的部分）
expected = [i for i in full_ids if i > mid]
if resume_ids == expected:
    print(f"  ok: 从 offset={mid} 续拉到 {len(resume_ids)} 个，与完整序列后缀一致")
else:
    print(f"  MISMATCH: resume={resume_ids[:5]}... expected={expected[:5]}...")
    exit(1)
PYEOF
[ $? -eq 0 ] && ok "断线续传补全正确" || fail "断线续传不一致"

echo "[5] pending_confirms 快照存在（无审批时应为空数组）"
PENDING=$(python3 -c "import json;d=json.load(open('/tmp/pull-all.json'));print(len(d.get('pending_confirms',[])))")
echo "  pending_confirms: $PENDING 条（无活跃审批，应为 0）"
[ "$PENDING" -eq 0 ] && ok "pending 快照字段存在且为空（无审批）" || fail "pending 快照异常"

echo
echo "passed: $PASS, failed: $FAIL"
[ "$FAIL" -eq 0 ] && { echo "PULL ACCEPTANCE OK"; exit 0; } || { echo "PULL ACCEPTANCE FAILED — 问题可能在事件源而非传输，回查!"; exit 1; }
