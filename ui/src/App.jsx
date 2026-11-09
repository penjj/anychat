import { useCallback, useEffect, useRef, useState } from 'react'

const invoke = window.__TAURI__.core.invoke
const events = window.__TAURI__.event

const GROUP_NAMES = { global: '国际', cn: '国内', build: '构建' }
const GROUP_ORDER = ['global', 'cn', 'build']
const PINNED_COUNT = 10

/** Below this the labels stop fitting and the rail becomes an icon strip.
 *  Derived from width on purpose: a stored flag could disagree with reality. */
const ICON_ONLY_BELOW = 88
const RAIL_MIN = 52
const RAIL_MAX = 420

const cl = (...parts) => parts.filter(Boolean).join(' ')

function initials (name) {
  return name.replace(/[^A-Za-z0-9\u4e00-\u9fa5]/g, '').slice(0, 2)
}

function hueFor (id) {
  let h = 0
  for (let i = 0; i < id.length; i += 1) h = (h * 31 + id.charCodeAt(i)) % 360
  return h
}

/** Most-used first; recency breaks ties, name keeps untouched tools stable. */
function byUsage (a, b) {
  if ((b.uses || 0) !== (a.uses || 0)) return (b.uses || 0) - (a.uses || 0)
  if ((b.last_used || 0) !== (a.last_used || 0)) return (b.last_used || 0) - (a.last_used || 0)
  return a.name.localeCompare(b.name)
}

/** Context menu for a tool row.
 *
 *  Reclaiming is destructive to page state -- the next visit reloads -- so the
 *  entry names the cost rather than hiding it behind "clear memory". The item
 *  is disabled for the active tool because destroying what is on screen would
 *  blank the window.
 */
function ToolMenu ({ menu, onClose, onReclaim, onReclaimAll }) {
  useEffect(() => {
    if (!menu) return
    const away = (e) => {
      if (e.target.closest('.ctx-menu')) return
      onClose()
    }
    const esc = (e) => { if (e.key === 'Escape') onClose() }
    window.addEventListener('mousedown', away)
    window.addEventListener('keydown', esc)
    // Losing focus means the menu can no longer be dismissed by clicking, so
    // close it. This also guarantees the content view is restored rather than
    // staying hidden behind a menu the user can no longer see.
    const blur = () => onClose()
    window.addEventListener('blur', blur)
    return () => {
      window.removeEventListener('mousedown', away)
      window.removeEventListener('keydown', esc)
      window.removeEventListener('blur', blur)
    }
  }, [menu, onClose])

  if (!menu) return null
  return (
    <div
      className="ctx-menu"
      style={{ left: menu.x, top: menu.y }}
      role="menu"
    >
      <button
        type="button"
        role="menuitem"
        disabled={menu.active}
        onClick={() => onReclaim(menu.id)}
      >
        释放此站内存
      </button>
      <button type="button" role="menuitem" onClick={onReclaimAll}>
        释放所有空闲站点
      </button>
      <div className="ctx-note">
        {menu.active ? '当前站点正在使用，不能释放' : '释放后再打开会重新加载'}
      </div>
    </div>
  )
}

/** Eight invisible hit targets around the window edge.
 *
 *  A borderless window loses macOS's own resize borders, and the injected
 *  corner radius clips what remains, so the native corners were a target of a
 *  few pixels. These strips make the whole edge draggable instead. Only the
 *  rail page can host them: the content area is a native child view above it.
 *
 *  The drag reports the offset from where it started, not per-move deltas, so a
 *  missed frame cannot accumulate rounding error.
 */
const EDGES = [
  ['nw', { left: 0, top: 0, width: 18, height: 18, cursor: 'nwse-resize' }],
  ['ne', { right: 0, top: 0, width: 18, height: 18, cursor: 'nesw-resize' }],
  ['sw', { left: 0, bottom: 0, width: 18, height: 18, cursor: 'nesw-resize' }],
  ['se', { right: 0, bottom: 0, width: 18, height: 18, cursor: 'nwse-resize' }],
  ['n', { left: 18, right: 18, top: 0, height: 6, cursor: 'ns-resize' }],
  ['s', { left: 18, right: 18, bottom: 0, height: 6, cursor: 'ns-resize' }],
  ['w', { left: 0, top: 18, bottom: 18, width: 6, cursor: 'ew-resize' }],
  ['e', { right: 0, top: 18, bottom: 18, width: 6, cursor: 'ew-resize' }],
]

function ResizeEdges () {
  const state = useRef(null)

  useEffect(() => {
    const move = (e) => {
      const s = state.current
      if (!s) return
      invoke('resize_window', {
        corner: s.corner,
        dx: e.screenX - s.x,
        dy: e.screenY - s.y,
      }).catch(() => {})
    }
    const up = () => { state.current = null }
    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', up)
    window.addEventListener('pointercancel', up)
    return () => {
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', up)
      window.removeEventListener('pointercancel', up)
    }
  }, [])

  return EDGES.map(([corner, style]) => (
    <div
      key={corner}
      className="resize-edge"
      data-corner={corner}
      style={{ position: 'fixed', zIndex: 40, ...style }}
      onPointerDown={(e) => {
        e.preventDefault()
        e.stopPropagation()
        state.current = { corner, x: e.screenX, y: e.screenY }
      }}
    />
  ))
}

function Icon ({ id, className }) {
  return (
    <svg className={cl('ico', className)} aria-hidden="true">
      <use href={`#i-${id}`} />
    </svg>
  )
}

/** Mean luminance of an icon, 0-255, or null when it cannot be read.
 *
 *  Site favicons are authored for a white page, so a near-black mark (GitHub,
 *  OpenAI) is invisible on a dark rail while a bright one is invisible on a
 *  light rail. Measuring lets the plate appear only where it is needed instead
 *  of washing out every icon in the list.
 */
const lumCache = new Map()

function iconLuminance (src) {
  if (lumCache.has(src)) return lumCache.get(src)
  lumCache.set(src, null) // in flight: do not start a second decode
  const img = new Image()
  img.onload = () => {
    try {
      const c = document.createElement('canvas')
      c.width = 16
      c.height = 16
      const ctx = c.getContext('2d')
      ctx.drawImage(img, 0, 0, 16, 16)
      const { data } = ctx.getImageData(0, 0, 16, 16)
      let sum = 0
      let n = 0
      for (let i = 0; i < data.length; i += 4) {
        if (data[i + 3] < 40) continue
        sum += 0.299 * data[i] + 0.587 * data[i + 1] + 0.114 * data[i + 2]
        n += 1
      }
      lumCache.set(src, n ? sum / n : null)
      window.dispatchEvent(new Event('anychat:icon-lum'))
    } catch (_) {
      lumCache.set(src, null)
    }
  }
  img.onerror = () => lumCache.set(src, null)
  img.src = src
  return null
}

function Mark ({ tool, broken, onBreak }) {
  const [, bump] = useState(0)
  useEffect(() => {
    const f = () => bump((n) => n + 1)
    window.addEventListener('anychat:icon-lum', f)
    return () => window.removeEventListener('anychat:icon-lum', f)
  }, [])

  if (tool.icon && !broken) {
    const lum = iconLuminance(tool.icon)
    // Either extreme needs a plate: too dark for a dark rail, too bright for a
    // light one. Mid-range marks carry their own contrast and get none.
    const needsPlate = lum !== null && (lum < 70 || lum > 200)
    return (
      <img
        className={cl('favicon', needsPlate && 'on-plate')}
        src={tool.icon}
        alt=""
        onError={() => onBreak(tool.id)}
      />
    )
  }
  return (
    <span className="mark-initial" style={{ '--mark-hue': hueFor(tool.id) }}>
      {initials(tool.name)}
    </span>
  )
}

function ToolRow ({ tool, active, broken, onSelect, onBreak, onContext }) {
  return (
    <div
      className={cl('tool', active && 'active')}
      data-id={tool.id}
      title={tool.name}
      onClick={() => onSelect(tool.id)}
      onContextMenu={(e) => {
        e.preventDefault()
        onContext(tool.id, e.clientX, e.clientY)
      }}
    >
      <span className="mark">
        <Mark tool={tool} broken={broken} onBreak={onBreak} />
      </span>
      <span className="name">{tool.name}</span>
    </div>
  )
}

export default function App () {
  const [state, setState] = useState({
    tools: [],
    active: null,
    active_name: '',
    rust_url: '',
    autostart: false,
    shortcut_ok: true,
    rail_width: 208,
    always_on_top: false,
    zoom: 0.8,
    settings_open: false,
    theme: 'auto',
  })
  // The theme is authoritative in Rust; the pre-paint script in index.html only
  // has localStorage. Keep the two in step so a relaunch does not flash.
  useEffect(() => {
    if (!state.theme) return
    document.documentElement.setAttribute('data-theme', state.theme)
    try {
      localStorage.setItem('anychat:theme', state.theme)
    } catch (_) {}
  }, [state.theme])

  const [liveUrl, setLiveUrl] = useState('')
  const [broken, setBroken] = useState(() => new Set())
  const [menu, setMenu] = useState(null)
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [dragging, setDragging] = useState(false)
  const [railWidth, setRailWidth] = useState(208)

  const slotRef = useRef(null)
  const railRef = useRef(null)
  const lastBounds = useRef('')
  const dragWidth = useRef(208)
  /* Pending animation frame for the native content view during a drag. */
  const moveFrame = useRef(0)

  const iconOnly = railWidth < ICON_ONLY_BELOW

  /* ---------- derived ---------- */

  const pinnedTools = state.tools.slice().sort(byUsage).slice(0, PINNED_COUNT)
  const pinnedIds = new Set(pinnedTools.map((t) => t.id))

  const grouped = GROUP_ORDER
    .map((g) => ({
      group: g,
      items: state.tools.filter((t) => t.group === g && !pinnedIds.has(t.id)).sort(byUsage),
    }))
    .filter((g) => g.items.length)

  /* ---------- geometry ---------- */

  const reportBounds = useCallback(() => {
    const el = slotRef.current
    if (!el) return
    const r = el.getBoundingClientRect()
    const rect = {
      x: Math.round(r.left),
      y: Math.round(r.top),
      width: Math.round(r.width),
      height: Math.round(r.height),
    }
    const key = `${rect.x}:${rect.y}:${rect.width}:${rect.height}`
    if (key === lastBounds.current) return
    lastBounds.current = key
    invoke('layout', { rect })
  }, [])

  const reportReady = useCallback(() => {
    const slot = slotRef.current
    const rail = railRef.current
    if (!slot || !rail) return
    const r = slot.getBoundingClientRect()
    const head = document.querySelector('.rail-head')?.getBoundingClientRect()
    const pin = document.getElementById('btn-top')?.getBoundingClientRect()
    const hide = document.getElementById('btn-hide')?.getBoundingClientRect()

    invoke('rail_ready', {
      report: JSON.stringify({
        tools: document.querySelectorAll('.tool').length,
        groups: document.querySelectorAll('.group').length,
        active: document.querySelector('.tool.active')?.dataset?.id ?? null,
        slot: { w: Math.round(r.width), h: Math.round(r.height) },
        icons: document.querySelectorAll('.tool img.favicon').length,
        railWidth: Math.round(rail.getBoundingClientRect().width),
        railCardWidth: Math.round(rail.getBoundingClientRect().width),
        // Proves the label sits centred rather than flush left: compare the gap
        // on each side of the button's content box.
        footBtn: (() => {
          const btn = document.getElementById('btn-new')
          if (!btn) return null
          const b = btn.getBoundingClientRect()
          const kids = Array.from(btn.children).map((n) => n.getBoundingClientRect())
          if (!kids.length) return null
          const left = Math.min(...kids.map((k) => k.left))
          const right = Math.max(...kids.map((k) => k.right))
          return {
            gapL: Math.round(left - b.left),
            gapR: Math.round(b.right - right),
            padL: parseFloat(getComputedStyle(btn).paddingLeft) || 0,
            padR: parseFloat(getComputedStyle(btn).paddingRight) || 0,
          }
        })(),
        mainWidth: Math.round(document.getElementById('main')?.getBoundingClientRect().width ?? 0),
        bodyClass: document.body.className,
        railVar: getComputedStyle(document.documentElement).getPropertyValue('--rail-w').trim(),
        iconOnly,
        pinned: pinnedTools.length,
        pinnedLabels: document.querySelectorAll('.group')[0]?.textContent === '常用' ? 1 : 0,
        // Computed visibility, not just presence: the captions must actually be
        // gone when the rail collapses, and `display:none` is what guarantees it.
        visibleGroups: Array.from(document.querySelectorAll('.group'))
          .filter((n) => getComputedStyle(n).display !== 'none').length,
        // Reported so the gate can prove the menu actually renders, with the
        // right items, rather than trusting that a click handler exists.
        // Geometry of the first row, so a real click can be aimed at it.
        // The edge targets are real DOM nodes; report them so a broken layout
        // shows up as zero rects rather than as "the drag silently did nothing".
        menu: (() => {
          const m = document.querySelector('.ctx-menu')
          if (!m) return null
          const r = m.getBoundingClientRect()
          return {
            w: Math.round(r.width),
            h: Math.round(r.height),
            items: Array.from(m.querySelectorAll('button')).map(
              (b) => b.textContent.trim() + (b.disabled ? '(disabled)' : '')),
          }
        })(),
        duplicateTools: (() => {
          const ids = Array.from(document.querySelectorAll('.tool')).map((n) => n.dataset.id)
          return ids.length - new Set(ids).size
        })(),
        app: { w: Math.round(window.innerWidth), h: Math.round(window.innerHeight) },
        win: { w: window.innerWidth, h: window.innerHeight },
        settingsPanel: Boolean(document.querySelector('#settings-view .settings-card')),
        settingsHidden: document.getElementById('settings-view')?.hidden ?? true,
        chrome: head && pin && hide
          ? {
              barH: Math.round(document.getElementById('bar')?.getBoundingClientRect().height ?? 0),
              pinVisible: pin.width > 0 && pin.height > 0,
              hideVisible: hide.width > 0 && hide.height > 0,
              hideAfterPin: Math.round(hide.left) > Math.round(pin.left),
              gapToEdge: Math.round((document.getElementById('bar')?.getBoundingClientRect().right ?? 0) - hide.right),
            }
          : null,
      }),
    }).catch(() => {})
  }, [iconOnly])

  /* ---------- effects ---------- */

  // The rail owns the layout, so it must report every geometry change. Observing
  // `#main` catches window resizes and rail drags with one mechanism.
  useEffect(() => {
    const main = document.getElementById('main')
    if (!main) return
    const ro = new ResizeObserver(() => reportBounds())
    ro.observe(main)
    const onResize = () => reportBounds()
    window.addEventListener('resize', onResize)
    return () => {
      ro.disconnect()
      window.removeEventListener('resize', onResize)
    }
  }, [reportBounds])

  useEffect(() => {
    reportBounds()
  }, [railWidth, reportBounds])

  // Report after paint so the gate reads settled geometry, not a mid-transition
  // snapshot. The extra timeout covers the CSS width transition.
  useEffect(() => {
    const a = requestAnimationFrame(reportReady)
    const b = setTimeout(reportReady, 260)
    return () => {
      cancelAnimationFrame(a)
      clearTimeout(b)
    }
  }, [railWidth, state.tools.length, state.active, broken, reportReady])

  /* ---------- backend state ---------- */

  const adoptState = useCallback((payload) => {
    if (!payload) return
    setState((prev) => ({ ...prev, ...payload }))
    if (typeof payload.rail_width === 'number') {
      setRailWidth(payload.rail_width)
      dragWidth.current = payload.rail_width
      document.documentElement.style.setProperty('--rail-w', payload.rail_width + 'px')
    }
    if (payload.settings_open) setSettingsOpen(true)
  }, [])

  useEffect(() => {
    invoke('state').then((payload) => {
      adoptState(payload)
      setTimeout(reportReady, 300)
    }).catch(() => {})

    let alive = true
    const unlisten = []

    events.listen('state', (e) => { if (alive) adoptState(e.payload) })
      .then((f) => alive && unlisten.push(f))
      .catch(() => {})

    events.listen('icons-ready', () => {
      if (!alive) return
      invoke('state').then((payload) => { adoptState(payload); reportReady() }).catch(() => {})
    }).then((f) => alive && unlisten.push(f)).catch(() => {})

    events.listen('rail-layout', (e) => {
      if (!alive) return
      const w = e.payload?.rail_width
      if (typeof w === 'number') {
        setRailWidth(w)
        dragWidth.current = w
        document.documentElement.style.setProperty('--rail-w', w + 'px')
      }
    }).then((f) => alive && unlisten.push(f)).catch(() => {})

    // wry's event delivery into a child webview is not reliable on macOS, so a
    // slow poll is the authoritative signal. It is deliberately cheap: one IPC
    // round trip per second, no DOM work unless something changed.
    const poll = setInterval(() => {
      invoke('state')
        .then((payload) => {
          if (!alive) return
          adoptState(payload)
          if (!payload.settings_open) setSettingsOpen(false)
        })
        .catch(() => {})
    }, 700)

    return () => {
      alive = false
      clearInterval(poll)
      unlisten.forEach((f) => { try { f() } catch (_) {} })
    }
  }, [adoptState, reportReady])

  /* ---------- actions ---------- */

  const refreshUrl = useCallback(() => {
    invoke('current_url')
      .then((u) => setLiveUrl(u || ''))
      .catch(() => setLiveUrl(''))
  }, [])

  useEffect(() => {
    const a = setTimeout(refreshUrl, 900)
    return () => clearTimeout(a)
  }, [state.active, refreshUrl])

  const selectTool = useCallback(async (id) => {
    try {
      await invoke('select_tool', { id })
      const payload = await invoke('state')
      adoptState(payload)
      setTimeout(refreshUrl, 700)
    } catch (_) {}
  }, [adoptState, refreshUrl])

  const openMenu = useCallback((id, x, y) => {
    setMenu({ id, x, y, active: id === state.active })
    // The content area is a native child view above the rail's page, so the
    // menu would render underneath it. Hiding that view is the only way to
    // bring the popup to the front.
    invoke('menu_open').catch(() => {})
  }, [state.active])

  /** Every exit path must restore the content view, or the window stays blank. */
  const closeMenu = useCallback(() => {
    setMenu(null)
    invoke('menu_close').catch(() => {})
  }, [])

  const reclaimOne = useCallback(async (id) => {
    closeMenu()
    try {
      await invoke('reclaim_tool', { id })
    } catch (_) {
      // The backend refuses the active tool; the menu already disables it, so a
      // rejection here means the row went stale between open and click.
    }
  }, [])

  const reclaimAll = useCallback(async () => {
    closeMenu()
    try {
      await invoke('reclaim_all_idle')
    } catch (_) {}
  }, [])

  const newChat = useCallback(() => {
    invoke('new_chat').then(() => setTimeout(refreshUrl, 700)).catch(() => {})
  }, [refreshUrl])

  const nav = useCallback((action) => {
    invoke('nav', { action }).then(() => setTimeout(refreshUrl, 500)).catch(() => {})
  }, [refreshUrl])

  // The settings panel lives in this page, so only this page can describe it.
  // Reported a few times because it mounts and animates into place.
  const reportSettings = useCallback(() => {
    const card = document.querySelector('#settings-view .settings-card')
    const view = document.getElementById('settings-view')
    if (!card || !view) return
    const r = card.getBoundingClientRect()
    invoke('settings_ready', {
      report: JSON.stringify({
        card: true,
        visible: !view.hidden && r.width > 0,
        rect: { w: Math.round(r.width), h: Math.round(r.height) },
        viewport: { w: window.innerWidth, h: window.innerHeight },
        autostart: Boolean(state.autostart),
        settingsHidden: view.hidden,
      }),
    }).catch(() => {})
  }, [state.autostart])

  useEffect(() => {
    if (!settingsOpen) return
    const timers = [80, 300, 800].map((d) => setTimeout(reportSettings, d))
    return () => timers.forEach(clearTimeout)
  }, [settingsOpen, reportSettings])

  const openSettings = useCallback(async () => {
    setSettingsOpen(true)
    try {
      const payload = await invoke('open_settings')
      adoptState(payload)
    } catch (_) {}
  }, [adoptState])

  const closeSettings = useCallback(async () => {
    setSettingsOpen(false)
    try {
      const payload = await invoke('close_settings')
      adoptState(payload)
    } catch (_) {}
  }, [adoptState])

  const applyZoom = useCallback((delta) => {
    const next = Math.round((state.zoom + delta) * 100) / 100
    invoke('set_zoom', { zoom: next, id: null })
      .then((layout) => setState((p) => ({ ...p, zoom: layout.zoom })))
      .catch(() => {})
  }, [state.zoom])

  const togglePin = useCallback(async () => {
    // Through the backend: this page is a child webview, so the window APIs do
    // not address the OS window, and the ACL does not grant them anyway. The
    // old call was rejected and its error swallowed, which is why the button
    // looked dead.
    const next = !state.always_on_top
    try {
      const actual = await invoke('set_pin', { pinned: next })
      setState((p) => ({ ...p, always_on_top: actual }))
    } catch (_) {}
  }, [state.always_on_top])

  const hideWindow = useCallback(() => {
    window.__TAURI__.window.getCurrentWindow().hide().catch(() => {})
  }, [])

  const breakIcon = useCallback((id) => {
    setBroken((prev) => {
      if (prev.has(id)) return prev
      const next = new Set(prev)
      next.add(id)
      return next
    })
  }, [])

  /* ---------- rail drag ---------- */

  const widthAt = (clientX) => Math.round(Math.min(RAIL_MAX, Math.max(RAIL_MIN, clientX)))

  const onPointerDown = (e) => {
    setDragging(true)
    e.currentTarget.setPointerCapture?.(e.pointerId)
  }

  const onPointerMove = (e) => {
    if (!dragging) return
    const w = widthAt(e.clientX)
    if (w === dragWidth.current) return
    dragWidth.current = w

    // During the drag only the CSS variable changes: one style write, no React
    // render, no `invoke('layout')`. Routing every pointermove through state
    // made each frame re-render the rail and then move the native content
    // webview via the ResizeObserver, which is what produced the judder -- the
    // view and the page were resynchronising at different times.
    document.documentElement.style.setProperty('--rail-w', w + 'px')

    // The content webview is a native view, so it cannot follow CSS. Move it on
    // an animation frame with the same geometry the page just used, so the page
    // and the view land together instead of chasing each other.
    if (moveFrame.current) cancelAnimationFrame(moveFrame.current)
    moveFrame.current = requestAnimationFrame(() => {
      const el = slotRef.current
      if (!el) return
      const r = el.getBoundingClientRect()
      invoke('layout', {
        rect: {
          x: Math.round(r.left),
          y: Math.round(r.top),
          width: Math.round(r.width),
          height: Math.round(r.height),
        },
      }).catch(() => {})
    })
  }

  const finishDrag = useCallback(async (clientX) => {
    if (!dragging) return
    setDragging(false)
    if (moveFrame.current) {
      cancelAnimationFrame(moveFrame.current)
      moveFrame.current = null
    }
    const w = widthAt(clientX ?? dragWidth.current)
    try {
      const layout = await invoke('set_rail_width', { width: w })
      setRailWidth(layout.rail_width)
      dragWidth.current = layout.rail_width
      document.documentElement.style.setProperty('--rail-w', layout.rail_width + 'px')
    } catch (_) {}
  }, [dragging])

  /* ---------- keyboard ---------- */

  useEffect(() => {
    const onKey = (e) => {
      if (e.key === 'Escape' && settingsOpen) {
        e.preventDefault()
        closeSettings()
        return
      }
      const mod = e.metaKey || e.ctrlKey
      if (!mod) return
      const k = e.key.toLowerCase()
      if (k === 'n' && !e.shiftKey) { e.preventDefault(); newChat(); return }
      if (k === 'r') { e.preventDefault(); nav('reload'); return }
      if (k === '[') { e.preventDefault(); nav('back'); return }
      if (k === ']') { e.preventDefault(); nav('forward'); return }
      if (k === ',') { e.preventDefault(); openSettings() }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [newChat, nav, openSettings, closeSettings, settingsOpen])

  /* ---------- title bar drag ---------- */

  // `data-tauri-drag-region` relies on Tauri's injected script, which a child
  // webview never loads, so dragging is wired explicitly on empty space only.
  const startDrag = (e) => {
    if (e.button !== 0) return
    if (e.target.closest('button, input, a, .tool')) return
    window.__TAURI__.window.getCurrentWindow().startDragging().catch(() => {})
  }

  /* ---------- render ---------- */

  const host = (() => {
    if (!liveUrl) return ''
    try { return new URL(liveUrl).hostname.replace(/^www\./, '') } catch { return liveUrl }
  })()

  return (
    <>
      <ResizeEdges />
      <div id="app" className={cl(dragging && 'resizing', iconOnly && 'icon-only')}>
        <aside id="rail" ref={railRef}>
          <div className="rail-head" onMouseDown={startDrag}>
            <span className="win-title">AnyChat</span>
          </div>

          <nav id="tool-list" className="tool-list">
            {pinnedTools.length > 0 && (
              <>
                <div className="group">常用</div>
                {pinnedTools.map((t) => (
                  <ToolRow
                    key={`pin-${t.id}`}
                    tool={t}
                    active={t.id === state.active}
                    broken={broken.has(t.id)}
                    onSelect={selectTool}
                    onBreak={breakIcon}
                    onContext={openMenu}
                  />
                ))}
              </>
            )}
            {grouped.map(({ group, items }) => (
              <div key={group}>
                <div className="group">{GROUP_NAMES[group] || group}</div>
                {items.map((t) => (
                  <ToolRow
                    key={t.id}
                    tool={t}
                    active={t.id === state.active}
                    broken={broken.has(t.id)}
                    onSelect={selectTool}
                    onBreak={breakIcon}
                    onContext={openMenu}
                  />
                ))}
              </div>
            ))}
          </nav>

          <div className="rail-foot">
            <button id="btn-new" title="新会话 (⌘N)" onClick={newChat}>
              <Icon id="plus" />
              <span className="foot-label">新会话</span>
            </button>
          </div>

          <div
            id="resizer"
            title="拖动调整宽度"
            onPointerDown={onPointerDown}
            onPointerMove={onPointerMove}
            onPointerUp={(e) => finishDrag(e.clientX)}
            onPointerCancel={(e) => finishDrag(e.clientX)}
            onLostPointerCapture={(e) => finishDrag(e.clientX)}
          />
        </aside>

        <ToolMenu
          menu={menu}
          onClose={closeMenu}
          onReclaim={reclaimOne}
          onReclaimAll={reclaimAll}
        />

        <main id="main">
          <header id="bar" onMouseDown={startDrag}>
            <span id="cur-name" className="cur-name">{state.active_name || 'AnyChat'}</span>
            <span id="cur-url" className="cur-url" title={liveUrl}>{host}</span>
            <span className="spacer" />
            <button className="icon" title="后退" onClick={() => nav('back')}><Icon id="back" /></button>
            <button className="icon" title="前进" onClick={() => nav('forward')}><Icon id="forward" /></button>
            <button className="icon" title="刷新 (⌘R)" onClick={() => nav('reload')}><Icon id="reload" /></button>
            <button className="icon" title="缩小页面 (⌘-)" onClick={() => applyZoom(-0.05)}><Icon id="minus" /></button>
            <button className="icon" title="放大页面 (⌘+)" onClick={() => applyZoom(0.05)}><Icon id="plus" /></button>
            <button
              className="icon"
              title="在浏览器打开"
              onClick={() => invoke('current_url').then((u) => u && invoke('open_external', { url: u })).catch(() => {})}
            >
              <Icon id="external" />
            </button>
            <button className="icon" title="设置" onClick={openSettings}><Icon id="settings" /></button>
            <span className="win-ctl">
              <button
                id="btn-top"
                className={cl('icon', state.always_on_top && 'active')}
                title={state.always_on_top ? '取消置顶' : '置顶窗口'}
                onClick={togglePin}
              >
                <Icon id="pin" />
              </button>
              <button id="btn-hide" className="icon" title="隐藏窗口 (⌘W)" onClick={hideWindow}>
                <Icon id="close" />
              </button>
            </span>
          </header>
          <div id="stage"><div id="slot" ref={slotRef} /></div>
          <div id="progress" />
        </main>
      </div>

      <div id="settings-view" hidden={!settingsOpen}>
        <div className="settings-card">
          <h2>设置</h2>
          <label className="row">
            <span>开机自启</span>
            <input
              type="checkbox"
              checked={Boolean(state.autostart)}
              onChange={async (e) => {
                const want = e.target.checked
                try {
                  const actual = await invoke('set_autostart', { enabled: want })
                  setState((p) => ({ ...p, autostart: Boolean(actual) }))
                } catch (err) {
                  // A dev build refuses to write the launch agent, because it
                  // would register target/debug and clobber the installed app's
                  // entry. Revert the checkbox and say why rather than leaving
                  // it ticked with nothing behind it.
                  setState((p) => ({ ...p, autostart: !want }))
                  if (String(err).includes('dev')) {
                    window.alert('开发版不写入开机自启，请在安装后的 AnyChat.app 中设置')
                  }
                }
              }}
            />
          </label>
          <div className="row">
            <span>主题</span>
            <span className="seg" role="radiogroup" aria-label="主题">
              {[
                ['auto', '跟随系统'],
                ['light', '浅色'],
                ['dark', '深色'],
              ].map(([value, label]) => (
                <button
                  key={value}
                  role="radio"
                  aria-checked={state.theme === value}
                  className={cl('seg-btn', state.theme === value && 'active')}
                  onClick={async () => {
                    // Optimistic: the theme is pure presentation, so waiting for
                    // the disk write would make the click feel dead. Roll back
                    // if the backend rejects the value.
                    const before = state.theme
                    setState((p) => ({ ...p, theme: value }))
                    try {
                      const actual = await invoke('set_theme', { theme: value })
                      setState((p) => ({ ...p, theme: actual }))
                    } catch (_) {
                      setState((p) => ({ ...p, theme: before }))
                    }
                  }}
                >
                  {label}
                </button>
              ))}
            </span>
          </div>
          <div className="row">
            <span>页面缩放</span>
            <span className="zoom-controls">
              <button title="缩小" onClick={() => applyZoom(-0.05)}><Icon id="minus" /></button>
              <span id="set-zoom">{Math.round(state.zoom * 100)}%</span>
              <button title="放大" onClick={() => applyZoom(0.05)}><Icon id="plus" /></button>
            </span>
          </div>
          <div className={cl('hint', state.shortcut_ok === false && 'warn')}>
            {state.shortcut_ok === false
              ? '⌥Space 被其他应用占用，注册失败。请改用托盘菜单。'
              : '⌥Space 显隐窗口（不抢焦点）· ⌘N 新会话 · ⌘R 刷新 · ⌘Q 退出'}
          </div>
          <div className="actions">
            <button className="primary" onClick={closeSettings}>完成</button>
          </div>
        </div>
      </div>
    </>
  )
}
