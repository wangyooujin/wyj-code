#!/usr/bin/env node
/**
 * i18n 键对齐检查（零依赖，node 20+）
 *
 * 为什么需要它：script.js 的 applyLang() 对缺失的键是**静默跳过**
 * （`if (dict[key] !== undefined)`）——不报错、不抛异常、页面上就是一块
 * 空白。zh/en 任一侧漏改、或 HTML 引用了不存在的键，都只能靠这个脚本发现。
 *
 * 检查三类问题：
 *   1. HTML 里 data-i18n 引用的键，在 zh / en 任一字典中缺失   → error
 *   2. zh 与 en 的键集合不对称                                  → error
 *   3. 字典里有键但没有任何 HTML 引用（死键）                   → warn
 *
 * 用法：node check-i18n.js   期望零输出即通过；接入 pages.yml 部署前。
 */
'use strict';

const fs = require('fs');
const path = require('path');

const SITE = __dirname;
const HTML_FILES = ['index.html', 'docs.html'];

/** 从 script.js 中稳健地取出 translations 对象字面量并 eval。
 *  用括号计数 + 字符串感知扫描定位结尾，避免按行号/缩进切分（缩进不一致会误报）。 */
function loadTranslations() {
  const src = fs.readFileSync(path.join(SITE, 'script.js'), 'utf8');
  const marker = 'var translations =';
  const start = src.indexOf(marker);
  if (start === -1) throw new Error('script.js 中找不到 "var translations ="');
  const open = src.indexOf('{', start);

  let depth = 0, i = open, quote = null;
  for (; i < src.length; i++) {
    const ch = src[i];
    if (quote) {
      if (ch === '\\') i++;            // 跳过转义字符
      else if (ch === quote) quote = null;
      continue;
    }
    if (ch === "'" || ch === '"' || ch === '`') { quote = ch; continue; }
    if (ch === '{') depth++;
    else if (ch === '}') {
      depth--;
      if (depth === 0) break;
    }
  }
  if (depth !== 0) throw new Error('translations 对象括号不匹配');
  // eslint-disable-next-line no-eval
  const dict = eval('(' + src.slice(open, i + 1) + ')');
  if (!dict.zh || !dict.en) throw new Error('字典缺少 zh 或 en 分支');
  return dict;
}

/** 收集 HTML 中所有 data-i18n 引用 */
function collectRefs() {
  const refs = new Map(); // key -> Set(filename)
  for (const f of HTML_FILES) {
    const p = path.join(SITE, f);
    if (!fs.existsSync(p)) continue;
    const html = fs.readFileSync(p, 'utf8');
    for (const m of html.matchAll(/data-i18n="([^"]+)"/g)) {
      if (!refs.has(m[1])) refs.set(m[1], new Set());
      refs.get(m[1]).add(f);
    }
  }
  return refs;
}

const problems = [];
const warnings = [];
const dict = loadTranslations();
const refs = collectRefs();
const zh = new Set(Object.keys(dict.zh));
const en = new Set(Object.keys(dict.en));

// 1. HTML 引用必须在两侧都存在
for (const [key, files] of refs) {
  const where = [...files].join(', ');
  if (!zh.has(key)) problems.push(`[${where}] data-i18n="${key}" 在 zh 字典中缺失`);
  if (!en.has(key)) problems.push(`[${where}] data-i18n="${key}" 在 en 字典中缺失`);
}

// 2. zh / en 键集必须完全一致
for (const k of zh) if (!en.has(k)) problems.push(`键 "${k}" 只在 zh 中，en 缺失`);
for (const k of en) if (!zh.has(k)) problems.push(`键 "${k}" 只在 en 中，zh 缺失`);

// 3. 死键
for (const k of zh) {
  if (!refs.has(k)) warnings.push(`死键 "${k}"：字典里有，但没有任何 HTML 引用`);
}

// 4. 空值
for (const [k, v] of Object.entries(dict.zh)) {
  if (typeof v !== 'string' || v.trim() === '') problems.push(`zh 键 "${k}" 的值为空`);
}
for (const [k, v] of Object.entries(dict.en)) {
  if (typeof v !== 'string' || v.trim() === '') problems.push(`en 键 "${k}" 的值为空`);
}

warnings.forEach((w) => console.warn(`warn  ${w}`));
problems.forEach((p) => console.error(`ERROR ${p}`));

console.log(
  `\n${HTML_FILES.length} 个页面, ${refs.size} 个引用, zh ${zh.size} 键, en ${en.size} 键 — ` +
  (problems.length ? `${problems.length} 个错误` : '键对齐通过')
);
process.exit(problems.length ? 1 : 0);
