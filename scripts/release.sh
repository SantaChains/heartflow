#!/usr/bin/env bash
# heartflow 发布脚本:本地与 CI 共用同一事实源,推上去之前先在本地验证,省 Actions 试错
#
# 用法:
#   bash scripts/release.sh plan               # dry 计算 released/bump/tag(只读,不改仓库)
#   bash scripts/release.sh publish            # 需 RELEASE_TAG/RELEASE_BUMP(来自 plan 输出)
#     RELEASE_DRY_RUN=1 bash scripts/release.sh publish   # 本地预演:改完版本号即还原,不 commit/不 push/不建 Release
#
# 版本语义(与 cliff.toml、AGENTS.md 对齐):
#   feat: → minor   fix: → patch   `!` 或 BREAKING CHANGE: → major
#   chore/docs/test/ci/style/build 等不触发发版
#
# 本地依赖:git-cliff(cargo install git-cliff --locked)、gh(仅 publish 真实模式需要)
set -euo pipefail

cd "$(cd "$(dirname "$0")/.." && pwd)"

log() {
  # 日志一律 ASCII:Windows Git-Bash 本地预检时中文会被 GBK 写坏,CI 日志同理
  LC_ALL=C printf '%s\n' "$*" >&2
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    log "missing dep: $1 (local install: cargo install git-cliff --locked; gh only needed by publish)"
    exit 2
  }
}

emit_output() { # key value —— CI 写 $GITHUB_OUTPUT,本地直接打印
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    printf '%s=%s\n' "$1" "$2" >>"$GITHUB_OUTPUT"
  else
    printf '%s=%s\n' "$1" "$2"
  fi
}

# 区间内是否存在版本相关提交(先看门控再要 git-cliff,本地无 cliff 也能验证 chore 短路)
range_has_release_commit() {
  local last="$1" log_src
  if [ -n "$last" ]; then
    log_src="$(git log "${last}..HEAD" --no-merges --pretty=%B)"
  else
    log_src="$(git log --no-merges --pretty=%B)"
  fi
  grep -qE '^(feat|fix)(\([^)]*\))?:|^[a-z]+(\([^)]*\))?!:|BREAKING CHANGE' <<<"$log_src"
}

do_plan() {
  local last base raw next l lm lmi n nm nmi bump
  last="$(git describe --tags --abbrev=0 2>/dev/null || true)"
  if ! range_has_release_commit "${last:-}"; then
    emit_output released false
    log "no release-related commits since ${last:-initial}, skip release"
    return 0
  fi
  need_cmd git-cliff
  base="${last:-v0.0.0}"
  raw="$(git-cliff --unreleased --bumped-version)"
  next="v${raw#v}"
  if [ "$next" = "$base" ]; then
    emit_output released false
    log "git-cliff produced no version increment, skip release"
    return 0
  fi
  l="${base#v}"; lm="${l%%.*}"; lmi="$(cut -d. -f2 <<<"$l")"
  n="${next#v}"; nm="${n%%.*}"; nmi="$(cut -d. -f2 <<<"$n")"
  if [ "$lm" != "$nm" ]; then bump=major
  elif [ "$lmi" != "$nmi" ]; then bump=minor
  else bump=patch; fi
  emit_output released true
  emit_output bump "$bump"
  emit_output tag "$next"
  log "next version: ${last:+${last} -> }${next} (${bump})"
}

do_publish() {
  local tag ver bump_label dry
  need_cmd git-cliff
  tag="${RELEASE_TAG:?缺少 RELEASE_TAG(来自 plan 输出)}"
  bump="${RELEASE_BUMP:?缺少 RELEASE_BUMP(来自 plan 输出)}"
  dry="${RELEASE_DRY_RUN:-0}"
  ver="${tag#v}"

  # 单一版本点:根 [workspace.package];各 crate 经 version.workspace = true 继承,禁止逐 crate 写死
  sed -i "0,/^version = \"/s/^version = \"[^\"]*\"/version = \"${ver}\"/" Cargo.toml
  # 只同步 workspace 内部 crate 在 Cargo.lock 中的版本,离线不触碰外部依赖
  cargo update --workspace --offline
  git-cliff --unreleased --bump -o release-notes.md

  if [ "$dry" = "1" ]; then
    git restore Cargo.toml Cargo.lock
    log "dry-run done: version calc + notes OK; Cargo.toml/Cargo.lock restored, no commit/push"
    log "----- release-notes.md preview -----"
    sed -n '1,20p' release-notes.md >&2
    return 0
  fi

  need_cmd gh
  git-cliff --unreleased --bump --prepend -o CHANGELOG.md
  git config user.name "${GIT_AUTHOR_NAME:-github-actions[bot]}"
  git config user.email "${GIT_AUTHOR_EMAIL:-418982825+github-actions[bot]@users.noreply.github.com}"
  git add Cargo.toml Cargo.lock CHANGELOG.md
  git commit -m "chore(release): ${tag} [skip ci]"
  git tag "$tag"
  git push origin "HEAD:main" "refs/tags/${tag}"
  case "$bump" in
    major) bump_label=主版本 ;;
    minor) bump_label=次版本 ;;
    *) bump_label=修订 ;;
  esac
  gh release create "$tag" \
    --target main \
    --title "heartflow ${tag} (${bump_label})" \
    --notes-file release-notes.md
  log "released ${tag}"
}

case "${1:-}" in
  plan) do_plan ;;
  publish) do_publish ;;
  *)
    log "usage: bash scripts/release.sh plan|publish"
    exit 1
    ;;
esac
