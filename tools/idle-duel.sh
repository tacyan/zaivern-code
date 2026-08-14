#!/usr/bin/env sh
# **アイドル時の CPU を、他のエディタと背中合わせで測る。**
#
# `tools/idle-cpu.sh` が「zai 単体の数字」を出すのに対し、こちらは
# **同じ機械・同じ瞬間・同じ手順**で比較対象と並べる。設計原則 3
# (アイドル時のコストはゼロ) が競合に対して本当に勝っているかは、
# **別々の瞬間に測った 2 つの数字を並べても分からない** (負荷の谷と山で跳ねる)。
#
# ## 使い方
#
#   tools/idle-duel.sh --vs Zed --out /tmp/duel.tsv
#   tools/idle-duel.sh --vs Zed --zai-a target/release/zai --zai-b /tmp/old-zai --out …
#   tools/idle-duel.sh --vs "Visual Studio Code" --rounds 1 --settle 20 --observe 20 --out …
#
#   --vs <アプリ名>      比較相手 (`open -a <名前>` で開けるもの)。省略すると zai だけ測る
#   --zai-a <パス>       測る zai (既定: target/release/zai)
#   --zai-b <パス>       もう 1 つの zai (版どうしの比較。省略可)
#   --workspace <パス>   開くフォルダ (既定: 一時ディレクトリに作る中立なもの)
#   --rounds <N>         繰り返し回数 (既定 3)。**中央値を取るために 1 回では出さない**
#   --settle <秒>        起動後に落ち着かせる時間 (既定 150)
#   --observe <秒>       観測する時間 (既定 180)
#   --retries <N>        無効だったときの再試行 (既定 3)
#   --tutorial seen|fresh 初回ガイドツアーを既読にするか (既定 seen)。
#                        `fresh` は**利用者が最初に見る経路**をそのまま測る
#   --out <パス>         結果の TSV (必須)
#
# ## ここが壊れていた、という記録 (同じ穴を掘り直さないために)
#
#   1. **背面のまま測っていた。** macOS は背面ウィンドウの描画を止めるので
#      「軽い」という嘘の数字が出る。→ 測る対象を最前面にしてから測り、
#      しかも **名前ではなく pid で照合する** (実測: 利用者自身の `zai` が
#      最前面に居て、名前照合では自分のものと見分けが付かなかった)
#   2. **人が使っている最中に測っていた。** `HIDIdleTime` が 0.2 秒だった。
#      → 人が居ないときだけ測り、観測の前後で `HIDIdleTime` の伸びを見て、
#      **途中で人が触ったらその測定を捨てる**
#   3. **二重起動。** 2 本のハーネスが互いのアプリを起動しては殺し合い、
#      `after > before` という**存在しない差**を作っていた。→ 多重起動を拒否する
#   4. **`syntax error` で途中死。** → `sh -n` を通してから使う (CI の
#      `installers + tools` ジョブが全 `tools/*.sh` を検査している)
#   5. **初回ガイドツアーが出たまま測っていた。** 32fps で回るので
#      測りたいアイドルの下限が埋もれる。→ 既読にしてから起動する。
#      ただし**既読にした経路しか測れない**と、利用者が最初に見る画面を
#      測らずに「軽い」と言うことになる。→ `--tutorial fresh` で両方測る
#   6. **`cpu_pct` を note 欄の中に埋めていた上に、書式と引数がずれていた。**
#      `cpu_pct=` が観測時刻を、`hid+` が CPU% を表示し、**人が触っていない
#      ことの証拠 (`grew`) は TSV に一度も書かれていなかった**。標準出力の
#      `echo` だけが正しかったので、端末を閉じると証拠が消えた。
#      → `cpu_pct` を独立した列にし、`grew` を必ず残す
#   7. **測定床を書いていなかった。** `ps -o time` は 1/100 秒までしか
#      持たないので、観測 180 秒なら 0.006% より下は区別できない。
#      床に張り付いた値を「実測 0.006%」と読んでしまう。→ `# floor` を出す
#
# ## 出さないもの
#
#   **合否の線を引かない。** 絶対時間の閾値は必ず嘘をつく (CLAUDE.md に実例 3 件)。
#   ここが出すのは生の数字と `INVALID` の理由だけで、勝ち負けは人が決める。
set -eu

# Windows (Git Bash / PowerShell) の既定コードページは UTF-8 ではないので、
# Python が日本語を stdout へ書いた瞬間に落ちる。ここでは python を使わないが、
# 他の tools と作法を揃えておく (`coordinator::tests::python3を呼ぶハーネスは…` 参照)。
export PYTHONUTF8="${PYTHONUTF8:-1}"
export PYTHONIOENCODING="${PYTHONIOENCODING:-utf-8}"

VS=""
WS=""
ZAI_A=""
ZAI_B=""
SETTLE=150
OBSERVE=180
ROUNDS=3
RETRIES=3
TUTORIAL=seen
OUT=""
# 人が居ないと見なすまでの空き時間 (秒)。短くすると「席を立った直後」を
# 拾ってしまい、まだ画面が動いている状態で測ることになる。
HUMAN_AWAY=${HUMAN_AWAY:-420}

# 行番号で切らない。説明を 1 行足すたびに使い方が黙って途中で切れる
# (実際に切れた)。先頭のコメント塊が終わるところまでを出す。
usage() { awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; }

while [ $# -gt 0 ]; do
    case "$1" in
    --vs) VS=$2; shift 2 ;;
    --workspace) WS=$2; shift 2 ;;
    --zai-a) ZAI_A=$2; shift 2 ;;
    --zai-b) ZAI_B=$2; shift 2 ;;
    --settle) SETTLE=$2; shift 2 ;;
    --observe) OBSERVE=$2; shift 2 ;;
    --rounds) ROUNDS=$2; shift 2 ;;
    --retries) RETRIES=$2; shift 2 ;;
    --tutorial) TUTORIAL=$2; shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    *) echo "知らない引数: $1 (--help で使い方)" >&2; exit 2 ;;
    esac
done

# **macOS 以外では声を出して降りる。** `HIDIdleTime` も `lsappinfo` も
# macOS の仕組みなので、他の OS では「人が居ないこと」を確かめられない。
# 黙って 0 件で緑にすると、測っていないのに測ったことになる。
if [ "$(uname -s)" != "Darwin" ]; then
    echo "[skip] このハーネスは macOS 専用です ($(uname -s) では人の在席を判定できません)" >&2
    echo "       Linux/Windows で測るなら、まず在席判定の代わりを用意すること。" >&2
    exit 0
fi

[ -n "$OUT" ] || { echo "--out が要ります (--help で使い方)" >&2; exit 2; }
case "$TUTORIAL" in
seen | fresh) ;;
*) echo "--tutorial は seen か fresh です: $TUTORIAL" >&2; exit 2 ;;
esac

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
[ -n "$ZAI_A" ] || ZAI_A="$root/target/release/zai"
[ -x "$ZAI_A" ] || { echo "zai がありません: $ZAI_A (cargo build --release --bin zai)" >&2; exit 2; }
[ -z "$ZAI_B" ] || [ -x "$ZAI_B" ] || { echo "zai がありません: $ZAI_B" >&2; exit 2; }

# **多重起動を拒否する。** 2 本走ると互いのアプリを殺し合い、
# 存在しない差を作る (実際に作った)。
lock="${TMPDIR:-/tmp}/zaivern-idle-duel.lock"
if ! mkdir "$lock" 2>/dev/null; then
    echo "別の idle-duel が走っています ($lock)。終わってから実行してください。" >&2
    echo "  古い残骸なら: rmdir '$lock'" >&2
    exit 2
fi

# **スクリーンセーバとディスプレイスリープは、このハーネスが自分で止める。**
# 操作者が別途 `caffeinate` を張る前提にすると、**期限切れに誰も気付けない**。
# 実測: 1 回目は手動の `caffeinate` の有効期限に偶然収まって 6/6 通り、
# 2 回目は期限切れ後に走って **既定 20 分のスクリーンセーバが観測窓に入り**、
# 1 件が INVALID になった。同じ手順の 2 回で結果が違うなら、それは手順ではない。
#
#   * `-u` は付けない。人の在席を偽装するので `HIDIdleTime` が潰れ、
#     このハーネスの在席ゲートそのものが機能しなくなる
#   * `-w $$` でこのスクリプトの寿命に縛る。異常終了しても道連れで消える
#     (`trap` に頼ると `kill -9` で取り残される)
if command -v caffeinate >/dev/null 2>&1; then
    caffeinate -dims -w $$ &
    echo "スクリーンセーバ/ディスプレイスリープを抑止しました (このハーネスの実行中のみ)"
else
    echo "警告: caffeinate がありません。既定 20 分のスクリーンセーバで測定が無効になることがあります" >&2
fi

# 中立なワークスペースを用意する (指定が無ければ)。
tmpws=""
if [ -z "$WS" ]; then
    tmpws=$(mktemp -d)
    WS="$tmpws"
    printf '# bench\n' >"$WS/README.md"
    for i in 1 2 3; do printf 'fn main() { println!("%s"); }\n' "$i" >"$WS/m$i.rs"; done
fi

cpu_seconds() {
    ps -o time= -p "$1" 2>/dev/null | awk '
        NF == 0 { next }
        { t = $1; d = 0
          if (index(t, "-") > 0) { split(t, a, "-"); d = a[1]; t = a[2] }
          n = split(t, p, ":"); s = 0
          for (i = 1; i <= n; i++) s = s * 60 + p[i]
          printf "%.3f\n", s + d * 86400; exit }'
}
rss_mb() { ps -o rss= -p "$1" 2>/dev/null | awk '{printf "%.1f\n", $1/1024; exit}'; }
alive() { kill -0 "$1" 2>/dev/null; }

# 最前面アプリの **pid**。名前だと死んだアプリの記録が残って嘘をつく
# (実測: zai を kill した後も lsappinfo が "zai" を返した)。
front_pid() {
    a=$(lsappinfo front 2>/dev/null) || { echo 0; return; }
    [ -n "$a" ] || { echo 0; return; }
    lsappinfo info -only pid "$a" 2>/dev/null | sed 's/.*=//; s/[^0-9]//g'
}
# 最前面アプリの**名前**。判定そのものは pid で行うが、名前が無いと
# **ロック/スクリーンセーバで loginwindow が前に出た**のか、単に別のアプリが
# 前に出たのかを区別できない (実測: 20 分の既定スクリーンセーバが起動したのを
# 「最前面でない (222 -> 222)」と報告し、原因の特定に 1 時間かかった)。
front_name() {
    a=$(lsappinfo front 2>/dev/null) || { echo "-"; return; }
    [ -n "$a" ] || { echo "-"; return; }
    n=$(lsappinfo info -only name "$a" 2>/dev/null | sed 's/.*=//; s/"//g')
    echo "${n:--}"
}
# 人が最後にキーボード/マウスへ触ってからの秒数。
hid_idle() {
    ioreg -c IOHIDSystem | awk -F'= ' '/HIDIdleTime/ {printf "%.0f\n", $2/1000000000; exit}'
}
locked() {
    a=$(lsappinfo front 2>/dev/null) || { echo yes; return; }
    n=$(lsappinfo info -only name "$a" 2>/dev/null | sed 's/.*=//; s/"//g')
    if [ "$n" = "loginwindow" ]; then echo yes; else echo no; fi
}

kill_apps() {
    [ -z "$VS" ] || osascript -e "tell application \"$VS\" to quit" >/dev/null 2>&1 || true
    pkill -f "$ZAI_A" 2>/dev/null || true
    [ -z "$ZAI_B" ] || pkill -f "$ZAI_B" 2>/dev/null || true
    sleep 4
}
cleanup() {
    kill_apps
    [ -z "$tmpws" ] || rm -rf "$tmpws"
    rmdir "$lock" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# 人が居なくなるまで待つ (最長 $1 秒)。居なくなれば 0。
wait_for_away() {
    limit=$1
    waited=0
    while [ "$waited" -lt "$limit" ]; do
        if [ "$(locked)" = no ] && [ "$(hid_idle)" -ge "$HUMAN_AWAY" ]; then
            return 0
        fi
        sleep 20
        waited=$((waited + 20))
    done
    return 1
}

invalid() {
    printf '%s\tINVALID\t-\t-\t-\t-\t-\t%s\n' "$1" "$2" >>"$OUT"
    echo "[$1] $2"
}

# $1=ラベル $2=app|bin $3=パスまたはアプリ名
measure() {
    label=$1; kind=$2; target=$3
    attempt=1
    while [ "$attempt" -le "$RETRIES" ]; do
        if ! wait_for_away 3600; then
            invalid "$label" "人が居る/ロック中で測れない (試行 $attempt)"
            attempt=$((attempt + 1)); continue
        fi
        kill_apps
        if [ "$kind" = app ]; then
            open -a "$target" "$WS"
            sleep 10
            pid=$(ps -Ao pid=,command= | awk -v n="$target" \
                'index($0, n ".app/Contents/MacOS") && !/crash-handler/ {print $1; exit}')
            osascript -e "tell application \"$target\" to activate" >/dev/null 2>&1 || true
        else
            ZH=$(mktemp -d)
            mkdir -p "$ZH/z"
            # **初回ガイドツアーを既読にするかどうかで、測る経路が変わる。**
            #   seen  — ツアーを飛ばした「2 回目以降」のアイドル (既定)。
            #           出したままだと 32fps で回り、測りたい下限が埋もれる
            #   fresh — 何も無いホームから起こした**初回起動そのもの**。
            #           利用者が最初に見る経路はこちらなので、`seen` だけを
            #           測って「アイドルは軽い」と言うと、いちばん多く使われる
            #           経路を測らずに主張したことになる
            if [ "$TUTORIAL" = seen ]; then
                printf 'done = true\nversion = 1\n' >"$ZH/z/tutorial.toml"
            fi
            ZAIVERN_HOME="$ZH/z" "$target" "$WS" >"$ZH/log" 2>&1 &
            pid=$!
            sleep 10
            osascript -e "tell application \"System Events\" to set frontmost of process \"$(basename "$target")\" to true" \
                >/dev/null 2>&1 || true
        fi
        sleep 3
        if [ -z "${pid:-}" ] || ! alive "$pid"; then
            invalid "$label" "起動できず (試行 $attempt)"
            attempt=$((attempt + 1)); continue
        fi
        sleep "$SETTLE"
        alive "$pid" || { invalid "$label" "落ち着かせ中に終了 (試行 $attempt)"; attempt=$((attempt + 1)); continue; }

        fp0=$(front_pid); fn0=$(front_name); hid0=$(hid_idle)
        t0=$(cpu_seconds "$pid"); s0=$(date +%H:%M:%S)
        sleep "$OBSERVE"
        alive "$pid" || { invalid "$label" "観測中に終了 (試行 $attempt)"; attempt=$((attempt + 1)); continue; }
        t1=$(cpu_seconds "$pid"); fp1=$(front_pid); fn1=$(front_name); hid1=$(hid_idle)
        rss=$(rss_mb "$pid"); s1=$(date +%H:%M:%S); grew=$((hid1 - hid0))

        # **ロック/スクリーンセーバを先に判定する。** どちらも loginwindow を
        # 最前面へ出すので、順序を逆にすると「最前面でない」という**間違った
        # 理由**が出る。実際にそれで誤診し、電源ログを 1 時間漁ってから
        # `targetUserIdle = 1200.0` (既定 20 分) に行き着いた。
        # 理由が違うと、直すべき場所も違ってしまう。
        if [ "$fn0" = loginwindow ] || [ "$fn1" = loginwindow ]; then
            invalid "$label" "観測中にロック/スクリーンセーバ (loginwindow が最前面) $s0..$s1 (試行 $attempt)"
            attempt=$((attempt + 1)); continue
        fi
        if [ "$fp0" != "$pid" ] || [ "$fp1" != "$pid" ]; then
            invalid "$label" "最前面でない ($fp0 $fn0 -> $fp1 $fn1、期待 $pid) $s0..$s1 (試行 $attempt)"
            attempt=$((attempt + 1)); continue
        fi
        if [ "$grew" -lt $((OBSERVE - 10)) ]; then
            invalid "$label" "観測中に人が触った (hid +${grew}s < ${OBSERVE}s) $s0..$s1 (試行 $attempt)"
            attempt=$((attempt + 1)); continue
        fi
        # (ロック判定は観測窓の両端で `fn0`/`fn1` として済ませた。ここでもう一度
        #  `locked` を呼ぶと、同じ瞬間を二度見るだけで新しいことは分からない)

        pct=$(awk -v a="$t0" -v b="$t1" -v s="$OBSERVE" 'BEGIN{printf "%.3f", (b-a)/s*100}')
        printf '%s\tVALID\t%s\t%s\t%s\t%s\t%s\twindow=%s hid+%ss\n' \
            "$label" "$pct" "$rss" "$t0" "$t1" "$pid" "$s0..$s1" "$grew" >>"$OUT"
        echo "[$label] cpu=${pct}% rss=${rss}MB $s0..$s1 (試行 $attempt)"
        kill_apps
        return 0
    done
    echo "[$label] $RETRIES 回とも無効でした"
    return 0
}

# **測定床を必ず一緒に出す。** `ps -o time` は 1/100 秒までしか持たないので、
# 観測 $OBSERVE 秒での最小の刻みがそのまま cpu_pct の下限になる。これを書いて
# おかないと、床に張り付いた値を「実測 0.006%」と読んでしまう (実際に読んだ)。
floor_pct=$(awk -v s="$OBSERVE" 'BEGIN{printf "%.3f", 0.01/s*100}')
{
    printf '# label\tstatus\tcpu_pct\trss_mb\tt0\tt1\tpid\tnote\n'
    printf '# gate\t人が %s 秒以上触っていない かつ 画面ロックなし のときだけ測る\n' "$HUMAN_AWAY"
    printf '# floor\tcpu_pct はこの値より下を区別できない: %s%% (ps の分解能 0.01 秒 / 観測 %s 秒)\n' \
        "$floor_pct" "$OBSERVE"
    printf '# tutorial\t%s\n' "$TUTORIAL"
    printf '# ws\t%s\n' "$WS"
} >"$OUT"

r=1
while [ "$r" -le "$ROUNDS" ]; do
    echo "=== round $r/$ROUNDS ($(date +%H:%M:%S)) ==="
    [ -z "$VS" ] || measure "vs.r$r" app "$VS"
    measure "zai-a.r$r" bin "$ZAI_A"
    [ -z "$ZAI_B" ] || measure "zai-b.r$r" bin "$ZAI_B"
    r=$((r + 1))
done

echo "--- 結果 ($OUT) ---"
cat "$OUT"
echo
echo "※ 合否の線はここでは引かない (絶対時間の閾値は必ず嘘をつく)。"
echo "   VALID の行だけを使い、ラウンドの中央値どうしで比べること。"
