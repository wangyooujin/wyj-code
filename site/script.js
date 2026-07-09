(function () {
  'use strict';

  var translations = {
    zh: {
      'nav.features': '特性',
      'nav.architecture': '架构',
      'nav.install': '安装',
      'nav.changelog': '更新日志',
      'nav.github': 'GitHub',

      'hero.kicker': '个人工程作品 · Clean-room 实现 · 开源',
      'hero.tagline': '用 Rust 从零实现的终端 AI 编程助手',
      'hero.desc': '单二进制、原生 TUI、双协议 LLM 适配（Anthropic / OpenAI）、多 Agent 编排、可插拔 MCP 工具链——在 shell 里直接与 AI 协作改代码，本地优先、全程透明可控、零遥测。',
      'hero.cta.github': '查看 GitHub 仓库',
      'hero.cta.install': '60 秒上手 →',
      'hero.badge.telemetry': '零遥测',

      'stats.size': 'release 二进制体积（已 strip）',
      'stats.boot': '稳态冷启动耗时',
      'stats.crates': 'workspace crate 数量',
      'stats.telemetry': '遥测 / 埋点上报',

      'features.kicker': 'Features',
      'features.title': '核心能力',
      'features.desc': '从推理循环到交互体验，每一层都是为终端场景重新设计的。',

      'feature.provider.title': '双协议 LLM 适配',
      'feature.provider.desc': 'Anthropic Messages 与 OpenAI Chat Completions 双格式原生支持，可混合配置：规划用一个模型，执行用另一个。',
      'feature.agent.title': '多 Agent 编排',
      'feature.agent.desc': '内置 general-purpose / Explore / Plan 三类子 Agent，支持自定义定义，进程级 Hub 管理并发、前后台调度与中断。',
      'feature.hooks.title': 'Hooks 生命周期自动化',
      'feature.hooks.desc': 'PreToolUse / PostToolUse / UserPromptSubmit / Stop 四个节点可挂任意 shell 脚本，对齐真实 Claude Code。',
      'feature.mcp.title': '可插拔 MCP',
      'feature.mcp.desc': '内置 Bash / Read / Write / Edit / Glob / Grep / WebFetch / TodoWrite，并可桥接任意外部 MCP server。',
      'feature.compact.title': '上下文自动压缩',
      'feature.compact.desc': 'token 数逼近窗口上限时自动生成摘要替换旧消息，长会话也不会突然断档。',
      'feature.memory.title': 'CLAUDE.md 记忆机制',
      'feature.memory.desc': '每轮重新读盘注入、子目录动态加载、@path 递归导入，跨会话记忆同样开箱即用。',
      'feature.tui.title': '原生 ratatui TUI',
      'feature.tui.desc': '流式 markdown、语法高亮、工具调用实时展示、子 Agent 聚合面板，交互体验为终端而生。',
      'feature.profile.title': 'Profile 分组配置',
      'feature.profile.desc': 'provider / model / base_url / api_key 具名分组，多供应商配置并存，随时切换。',
      'feature.session.title': '会话持久化',
      'feature.session.desc': '<code class="text-amber-400">-c</code> 续上次会话，<code class="text-amber-400">--resume &lt;id&gt;</code> 恢复任意历史会话。',
      'feature.slash.title': '自定义 Slash 命令',
      'feature.slash.desc': '兼容 <code class="text-amber-400">~/.claude/commands/*.md</code> 与项目级命令，六层路径合并加载。',
      'feature.i18n.title': '中 / 英双语',
      'feature.i18n.desc': '界面文案运行时切换语言，自动检测系统 locale，也可在配置中显式指定。',
      'feature.privacy.title': '零遥测',
      'feature.privacy.desc': '仅在显式调用 LLM / WebFetch / MCP 时才出网，没有任何隐式埋点或崩溃上报。',

      'arch.kicker': 'Architecture',
      'arch.title': '10 个 crate 的 Rust workspace',
      'arch.desc': '单一 wyj-code 二进制，职责分层清晰：从上到下依次是入口、服务、核心、基础四层。',
      'arch.layer.entry': '入口层 · Entry',
      'arch.cli': '二进制入口：组装全部 crate、解析 CLI 参数、启动 TUI / REPL / 单次问答模式',
      'arch.tui': 'ratatui 终端渲染：输入框、权限确认对话框、子 Agent 面板',
      'arch.layer.services': '服务层 · Services',
      'arch.tools': 'Read/Write/Edit/Bash/Glob/Grep/WebFetch/TodoWrite 等工具实现',
      'arch.commands': 'Slash 命令注册表与内置命令（/help、/compact 等）',
      'arch.mcp': 'MCP 客户端桥接（stdio / http 传输）',
      'arch.store': 'MCP / Skill / Plugin 安装元数据与市场客户端',
      'arch.i18n': '多语言资源与运行时语言切换',
      'arch.layer.core': '核心层 · Core',
      'arch.core': 'Agent 推理循环、Session、HistoryStore、MemoryStore、ClaudeMdLoader、上下文压缩',
      'arch.layer.foundation': '基础层 · Foundation',
      'arch.api': 'LLM Provider 抽象 trait + Anthropic/OpenAI 双格式实现，SSE 流式解析',
      'arch.config': '配置加载（~/.wyj-code/config.toml）、MCP 配置结构',

      'install.kicker': 'Install',
      'install.title': '60 秒上手',
      'install.desc': '克隆、构建、配置 API Key，直接开始对话。',
      'install.tab.prebuilt': '预编译安装包',
      'install.tab.source': '源码构建',
      'install.tab.dev': '开发者模式',
      'install.prebuilt.desc': '从 GitHub Releases 下载对应平台压缩包，解压后运行内置安装脚本，自动装入用户目录并配置 PATH，无需 sudo / 管理员权限。',
      'install.prebuilt.link': '前往 Releases 页面下载 →',
      'install.source.desc': '需要 Rust 1.80+ 工具链，构建 release 二进制并安装到 <code class="text-amber-400">~/.local/bin</code>。',
      'install.dev.desc': '直接用 cargo 跑起来，适合改代码调试。',
      'install.code.prebuilt': '<span class="text-paper/35"># macOS / Linux</span>\ntar xzf wyj-code-*.tar.gz &amp;&amp; cd wyj-code-*/ &amp;&amp; ./install.sh\n\n<span class="text-paper/35"># Windows（在解压目录里）</span>\ninstall.bat\n\n<span class="text-paper/35"># 之后升级</span>\nwyj-code update',
      'install.code.source': 'git clone https://github.com/wangyooujin/wyj-code.git\ncd wyj-code\n./build.sh install\n\n<span class="text-paper/35"># 卸载</span>\n./build.sh uninstall',
      'install.code.dev': '<span class="text-paper/35"># TUI 模式</span>\ncargo run\n\n<span class="text-paper/35"># 单次问答</span>\ncargo run -- -p "你的问题"\n\n<span class="text-paper/35"># headless REPL</span>\ncargo run -- --headless\n\n<span class="text-paper/35"># 查看配置状态</span>\ncargo run -- --config-status',

      'principles.kicker': 'Principles',
      'principles.title': '设计原则',
      'principle.local.title': '本地优先',
      'principle.local.desc': '没有隐式埋点、没有崩溃上报，配置与会话数据全部留在本机。',
      'principle.transparent.title': '透明可控',
      'principle.transparent.desc': '每一次工具调用全程实时展示，敏感操作前弹权限确认，随时可以 ESC 打断。',
      'principle.neutral.title': '协议中立',
      'principle.neutral.desc': '改一行 provider 配置即可在 Anthropic / OpenAI 之间切换，不锁定单一供应商。',
      'principle.zero.title': '零遥测',
      'principle.zero.desc': '只有显式的 LLM / WebFetch / MCP 调用才会出网，没有任何后台上报。',

      'changelog.kicker': 'Changelog',
      'changelog.title': '版本亮点',
      'changelog.latest': '最新',
      'changelog.v120': '自定义 Slash 命令对齐真实 Claude Code（识别 <code class="text-amber-400">~/.claude/commands/*.md</code>）；性能实测（12MB / ~10ms）与依赖排查；预编译包新增一键安装脚本 install.sh / install.bat。',
      'changelog.v110': '新增 Hooks 生命周期自动化系统（PreToolUse / PostToolUse / UserPromptSubmit / Stop）；开源产品化基线（LICENSE、CONTRIBUTING、CHANGELOG）。',
      'changelog.v102': '新增 <code class="text-amber-400">wyj-code update</code> 自更新；ExitPlanMode 计划面板重构；<code class="text-amber-400">build.sh release</code> 一键发版脚本。',
      'changelog.v100': '首发：Agent 推理循环、双协议 LLM 适配、内置工具集、ratatui TUI、子 Agent、上下文压缩、跨会话记忆、MCP / Skill / Plugin 市场。',
      'changelog.link': '查看完整更新日志 →',

      'cta.title': '在你的终端里试一下',
      'cta.desc': '开源、单二进制、零遥测——克隆下来跑起来只要 60 秒。',
      'cta.github': '前往 GitHub',
      'cta.install': '查看安装步骤',

      'footer.disclaimer': '个人技术作品集项目，基于公开的 Anthropic Messages API、OpenAI Chat Completions API 与 MCP 规范 clean-room 实现，不含任何第三方专有 prompt 或品牌资产，与 Anthropic / OpenAI 官方产品无关联。',

      'copy.copy': '复制',
      'copy.copied': '已复制',
    },
    en: {
      'nav.features': 'Features',
      'nav.architecture': 'Architecture',
      'nav.install': 'Install',
      'nav.changelog': 'Changelog',
      'nav.github': 'GitHub',

      'hero.kicker': 'Personal engineering project · Clean-room implementation · Open source',
      'hero.tagline': 'A terminal AI coding agent, built from scratch in Rust',
      'hero.desc': 'Single binary, native TUI, dual-protocol LLM support (Anthropic / OpenAI), multi-agent orchestration, pluggable MCP toolchain — collaborate with an AI on your code right in the shell. Local-first, fully transparent, zero telemetry.',
      'hero.cta.github': 'View on GitHub',
      'hero.cta.install': 'Get started in 60s →',
      'hero.badge.telemetry': 'Zero telemetry',

      'stats.size': 'release binary size (stripped)',
      'stats.boot': 'steady-state cold start',
      'stats.crates': 'crates in the workspace',
      'stats.telemetry': 'telemetry / analytics calls',

      'features.kicker': 'Features',
      'features.title': 'What it does',
      'features.desc': 'From the reasoning loop to the interaction model, every layer is designed for the terminal.',

      'feature.provider.title': 'Dual-protocol LLM support',
      'feature.provider.desc': 'Native support for both Anthropic Messages and OpenAI Chat Completions — mix providers, e.g. plan with one model, execute with another.',
      'feature.agent.title': 'Multi-agent orchestration',
      'feature.agent.desc': 'Built-in general-purpose / Explore / Plan sub-agents, plus custom definitions. A process-wide hub manages concurrency, foreground/background scheduling, and interrupts.',
      'feature.hooks.title': 'Hooks automation',
      'feature.hooks.desc': 'PreToolUse / PostToolUse / UserPromptSubmit / Stop — four lifecycle hooks that run arbitrary shell scripts, matching real Claude Code.',
      'feature.mcp.title': 'Pluggable MCP',
      'feature.mcp.desc': 'Bash / Read / Write / Edit / Glob / Grep / WebFetch / TodoWrite built in, plus a bridge to any external MCP server.',
      'feature.compact.title': 'Automatic context compaction',
      'feature.compact.desc': 'When the token count nears the context window limit, old messages are auto-summarized — long sessions never hit a hard wall.',
      'feature.memory.title': 'CLAUDE.md memory',
      'feature.memory.desc': 'Re-read from disk every turn, dynamic subdirectory loading, recursive @path imports — cross-session memory works out of the box.',
      'feature.tui.title': 'Native ratatui TUI',
      'feature.tui.desc': 'Streaming markdown, syntax highlighting, live tool-call rendering, an aggregated sub-agent panel — an interaction model built for the terminal.',
      'feature.profile.title': 'Named config profiles',
      'feature.profile.desc': 'provider / model / base_url / api_key grouped into named profiles — keep multiple providers configured and switch anytime.',
      'feature.session.title': 'Session persistence',
      'feature.session.desc': '<code class="text-amber-400">-c</code> resumes your last session, <code class="text-amber-400">--resume &lt;id&gt;</code> restores any session by id.',
      'feature.slash.title': 'Custom slash commands',
      'feature.slash.desc': 'Compatible with <code class="text-amber-400">~/.claude/commands/*.md</code> and project-level commands, merged across a six-tier path chain.',
      'feature.i18n.title': 'Bilingual UI',
      'feature.i18n.desc': 'Runtime language switching with system-locale auto-detection, or set it explicitly in config.',
      'feature.privacy.title': 'Zero telemetry',
      'feature.privacy.desc': 'The only network calls are explicit LLM / WebFetch / MCP requests — no implicit tracking, no crash reporting.',

      'arch.kicker': 'Architecture',
      'arch.title': 'A 10-crate Rust workspace',
      'arch.desc': 'One binary, cleanly layered responsibilities: entry, services, core, and foundation, top to bottom.',
      'arch.layer.entry': 'Entry layer',
      'arch.cli': 'Binary entry point: wires up every crate, parses CLI args, launches TUI / REPL / one-shot mode',
      'arch.tui': 'ratatui rendering: input box, permission dialogs, sub-agent panel',
      'arch.layer.services': 'Services layer',
      'arch.tools': 'Tool implementations: Read/Write/Edit/Bash/Glob/Grep/WebFetch/TodoWrite and more',
      'arch.commands': 'Slash command registry and built-ins (/help, /compact, etc.)',
      'arch.mcp': 'MCP client bridge (stdio / http transports)',
      'arch.store': 'Install metadata and marketplace clients for MCP / Skill / Plugin',
      'arch.i18n': 'Localization resources and runtime language switching',
      'arch.layer.core': 'Core layer',
      'arch.core': 'Agent reasoning loop, Session, HistoryStore, MemoryStore, ClaudeMdLoader, context compaction',
      'arch.layer.foundation': 'Foundation layer',
      'arch.api': 'LLM provider abstraction trait + Anthropic/OpenAI implementations, SSE stream parsing',
      'arch.config': 'Config loading (~/.wyj-code/config.toml), MCP config schema',

      'install.kicker': 'Install',
      'install.title': 'Up and running in 60 seconds',
      'install.desc': 'Clone, build, set your API key, start chatting.',
      'install.tab.prebuilt': 'Prebuilt binaries',
      'install.tab.source': 'Build from source',
      'install.tab.dev': 'Dev mode',
      'install.prebuilt.desc': 'Download the archive for your platform from GitHub Releases, unpack it, and run the bundled installer — it installs into your user directory and configures PATH, no sudo/admin required.',
      'install.prebuilt.link': 'Go to Releases →',
      'install.source.desc': 'Requires the Rust 1.80+ toolchain; builds a release binary and installs it to <code class="text-amber-400">~/.local/bin</code>.',
      'install.dev.desc': 'Run it straight from cargo — handy while hacking on the code.',
      'install.code.prebuilt': '<span class="text-paper/35"># macOS / Linux</span>\ntar xzf wyj-code-*.tar.gz &amp;&amp; cd wyj-code-*/ &amp;&amp; ./install.sh\n\n<span class="text-paper/35"># Windows (inside the extracted folder)</span>\ninstall.bat\n\n<span class="text-paper/35"># upgrade later</span>\nwyj-code update',
      'install.code.source': 'git clone https://github.com/wangyooujin/wyj-code.git\ncd wyj-code\n./build.sh install\n\n<span class="text-paper/35"># uninstall</span>\n./build.sh uninstall',
      'install.code.dev': '<span class="text-paper/35"># TUI mode</span>\ncargo run\n\n<span class="text-paper/35"># one-shot prompt</span>\ncargo run -- -p "your question"\n\n<span class="text-paper/35"># headless REPL</span>\ncargo run -- --headless\n\n<span class="text-paper/35"># check config status</span>\ncargo run -- --config-status',

      'principles.kicker': 'Principles',
      'principles.title': 'Design principles',
      'principle.local.title': 'Local-first',
      'principle.local.desc': 'No implicit tracking, no crash reporting — config and session data stay on your machine.',
      'principle.transparent.title': 'Transparent & controllable',
      'principle.transparent.desc': 'Every tool call is shown live; sensitive actions require confirmation; you can interrupt with ESC at any time.',
      'principle.neutral.title': 'Protocol-neutral',
      'principle.neutral.desc': 'Switch between Anthropic and OpenAI by changing one config line — never locked to a single provider.',
      'principle.zero.title': 'Zero telemetry',
      'principle.zero.desc': 'The only outbound calls are explicit LLM / WebFetch / MCP requests — nothing phones home.',

      'changelog.kicker': 'Changelog',
      'changelog.title': 'Release highlights',
      'changelog.latest': 'Latest',
      'changelog.v120': 'Custom slash commands aligned with real Claude Code (discovers <code class="text-amber-400">~/.claude/commands/*.md</code>); measured performance (12MB / ~10ms) with a dependency audit; prebuilt archives now bundle one-shot install.sh / install.bat.',
      'changelog.v110': 'Added the Hooks lifecycle automation system (PreToolUse / PostToolUse / UserPromptSubmit / Stop); open-source productization baseline (LICENSE, CONTRIBUTING, CHANGELOG).',
      'changelog.v102': 'Added <code class="text-amber-400">wyj-code update</code> self-update; reworked the ExitPlanMode plan panel; <code class="text-amber-400">build.sh release</code> one-shot release script.',
      'changelog.v100': 'Initial release: agent reasoning loop, dual-protocol LLM support, built-in toolset, ratatui TUI, sub-agents, context compaction, cross-session memory, MCP / Skill / Plugin marketplaces.',
      'changelog.link': 'Read the full changelog →',

      'cta.title': 'Try it in your terminal',
      'cta.desc': 'Open source, single binary, zero telemetry — clone it and you’re running in 60 seconds.',
      'cta.github': 'Go to GitHub',
      'cta.install': 'See install steps',

      'footer.disclaimer': 'A personal engineering portfolio project, clean-room implemented against the public Anthropic Messages API, OpenAI Chat Completions API, and MCP specifications. Contains no third-party proprietary prompts or brand assets, and is not affiliated with Anthropic or OpenAI.',

      'copy.copy': 'Copy',
      'copy.copied': 'Copied',
    },
  };

  var STORAGE_KEY = 'wyj-lang';

  function detectDefaultLang() {
    var saved = null;
    try {
      saved = localStorage.getItem(STORAGE_KEY);
    } catch (e) {}
    if (saved === 'zh' || saved === 'en') return saved;
    var nav = (navigator.language || 'zh').toLowerCase();
    return nav.indexOf('zh') === 0 ? 'zh' : 'en';
  }

  function applyLang(lang) {
    var dict = translations[lang] || translations.zh;
    document.documentElement.lang = lang === 'zh' ? 'zh-CN' : 'en';
    document.querySelectorAll('[data-i18n]').forEach(function (el) {
      var key = el.getAttribute('data-i18n');
      if (dict[key] !== undefined) el.innerHTML = dict[key];
    });
    document.querySelectorAll('.lang-btn').forEach(function (btn) {
      btn.classList.toggle('active', btn.getAttribute('data-lang') === lang);
    });
    try {
      localStorage.setItem(STORAGE_KEY, lang);
    } catch (e) {}
    window.__wyjLang = lang;
  }

  function initLang() {
    var lang = detectDefaultLang();
    applyLang(lang);
    document.querySelectorAll('.lang-btn').forEach(function (btn) {
      btn.addEventListener('click', function () {
        applyLang(btn.getAttribute('data-lang'));
      });
    });
  }

  function initTabs() {
    var buttons = document.querySelectorAll('.tab-btn');
    buttons.forEach(function (btn) {
      btn.addEventListener('click', function () {
        var target = btn.getAttribute('data-tab');
        buttons.forEach(function (b) { b.classList.toggle('active', b === btn); });
        document.querySelectorAll('[data-tab-panel]').forEach(function (panel) {
          panel.hidden = panel.getAttribute('data-tab-panel') !== target;
        });
      });
    });
  }

  function initCopyButtons() {
    document.querySelectorAll('.copy-btn').forEach(function (btn) {
      var targetId = btn.getAttribute('data-copy-target');
      var codeEl = document.getElementById(targetId);
      if (!codeEl) return;
      var originalIcon = btn.innerHTML;
      btn.addEventListener('click', function () {
        var text = codeEl.textContent;
        var done = function () {
          btn.innerHTML = '<svg viewBox="0 0 24 24" class="w-4 h-4"><use href="#i-check"/></svg>';
          setTimeout(function () { btn.innerHTML = originalIcon; }, 1600);
        };
        if (navigator.clipboard && navigator.clipboard.writeText) {
          navigator.clipboard.writeText(text).then(done).catch(function () { fallbackCopy(text); done(); });
        } else {
          fallbackCopy(text);
          done();
        }
      });
    });
  }

  function fallbackCopy(text) {
    var ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    try { document.execCommand('copy'); } catch (e) {}
    document.body.removeChild(ta);
  }

  function initReveal() {
    var items = document.querySelectorAll('.reveal');
    if (!('IntersectionObserver' in window)) {
      items.forEach(function (el) { el.classList.add('is-visible'); });
      return;
    }
    var observer = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (entry) {
          if (entry.isIntersecting) {
            entry.target.classList.add('is-visible');
            observer.unobserve(entry.target);
          }
        });
      },
      { threshold: 0.12, rootMargin: '0px 0px -40px 0px' }
    );
    items.forEach(function (el) { observer.observe(el); });
  }

  document.addEventListener('DOMContentLoaded', function () {
    initLang();
    initTabs();
    initCopyButtons();
    initReveal();
  });
})();
