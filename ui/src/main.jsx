import { createRoot } from 'react-dom/client'
import App from './App.jsx'
import './styles.css'

/* ---- TEMP DIAGNOSTICS -------------------------------------------------- */
/* Remove this block (and the one in App.jsx) after the Windows click issue  */
/* has been identified. Every line is prefixed [anychat] for easy filtering. */
const TAG = '[anychat]'
const LOG = (window.__ANYCHAT_LOG__ = window.__ANYCHAT_LOG__ || [])

function emit (...parts) {
  LOG.push({ at: new Date().toISOString().slice(11, 23), parts })
  console.info(TAG, ...parts)
}

// Paste this into the DevTools console to get a clean, copyable text log:
//   copy(__ANYCHAT_DUMP__())
window.__ANYCHAT_DUMP__ = () => LOG.map(({ at, parts }) =>
  `${at} ${parts.map((p) => {
    if (p === null) return 'null'
    if (p === undefined) return 'undefined'
    if (typeof p === 'object') {
      try { return JSON.stringify(p) } catch (_) { return String(p) }
    }
    return String(p)
  }).join(' ')}`
).join('\n')

function describe (el) {
  if (!el || el.nodeType !== 1) return null
  return {
    tag: el.tagName,
    id: el.id || '',
    cls: String(el.className || '').slice(0, 60),
    text: String(el.textContent || '').trim().slice(0, 32),
  }
}

const inTauri = Boolean(window.__TAURI_INTERNALS__)
emit('boot', {
  href: location.href,
  tauri: inTauri,
  hasGlobalTauri: Boolean(window.__TAURI__),
  theme: document.documentElement.getAttribute('data-theme'),
  platform: document.documentElement.getAttribute('data-platform'),
  viewport: [window.innerWidth, window.innerHeight],
  ua: navigator.userAgent,
})
if (!inTauri) {
  emit('BROWSER-ONLY: no Tauri runtime. Every invoke() will reject, the tool list stays empty, and settings switches roll back. The UI shell still renders -- this is expected, not a crash.')
}

window.addEventListener('error', (e) => {
  emit('window.error', {
    message: e.message, source: e.filename, line: e.lineno, error: String(e.error),
  })
})
window.addEventListener('unhandledrejection', (e) => {
  emit('unhandledrejection', String(e.reason))
})

/* Capture-phase hit probe. Records what the pointer landed on AND what the
   document really hit-tests at that coordinate. If a click never shows up in
   this log, a native layer (e.g. a child WebView) swallowed it before the DOM
   ever saw it -- which is the key distinction for "button does nothing". */
for (const type of ['pointerdown', 'mousedown', 'click']) {
  document.addEventListener(type, (e) => {
    const hit = document.elementFromPoint(e.clientX, e.clientY)
    emit('hit', {
      type,
      xy: [Math.round(e.clientX), Math.round(e.clientY)],
      target: describe(e.target),
      atPoint: describe(hit),
      matches: hit === e.target || Boolean(hit && e.target && hit.contains(e.target)),
      prevented: e.defaultPrevented,
    })
  }, true)
}
/* ---- END TEMP DIAGNOSTICS ---------------------------------------------- */

const root = document.getElementById('root')
createRoot(root).render(<App />)
emit('react render called')
