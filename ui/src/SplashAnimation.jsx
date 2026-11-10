import { useEffect, useRef, useState } from 'react'

/**
 * Launch animation: a field of drifting light gathers itself into the wordmark.
 *
 * The shape is measured, not drawn. The word is rasterised to an offscreen
 * canvas, its opaque pixels are sampled into target points, and every mote
 * flies to one of them — so the letters are made of the light itself, and come
 * out correctly proportioned at any window size.
 *
 * The word is carried by the motes alone: there is no crisp text layer on top,
 * so the sample has to be dense enough to read on its own. That is why the
 * point budget is high and every point gets its own mote — sharing a smaller
 * pool leaves holes in the letters.
 *
 * Timeline (≈3.0s):
 *   0.0–0.3  the field ignites; motes drift in from the edges
 *   0.3–1.6  motes curve inward and lock onto the letter shape
 *   1.6–2.4  hold, the word legible as light
 *   2.4–3.0  the field lifts and fades; the main window takes over
 *
 * Every beat below is expressed as a fraction of TOTAL or derived from it, so
 * changing the length does not leave a stale absolute somewhere in the middle
 * of the film. The handover does not depend on this timer at all -- the rail
 * releases the splash as soon as it can be used -- so this is the fallback for
 * a start slow enough that the film runs out first.
 */
const TOTAL = 3000
const TEXT = 'AnyChat'

/** Sample points to aim for. The word is drawn only by these motes, so the
 *  budget is set by legibility rather than by frame cost. */
const TARGET_POINTS = 28000
/** Free-floating motes that never join the word; they keep the air full during
 *  the gathering and clear out as the letters resolve. */
const AMBIENT = 1200

const HUE_BINS = 22
const ALPHA_BINS = 9
const HUE_LO = 182
const HUE_SPAN = 178

const clamp01 = (v) => (v < 0 ? 0 : v > 1 ? 1 : v)
const easeOutCubic = (t) => 1 - Math.pow(1 - t, 3)

function measureWord (width, height) {
  const off = document.createElement('canvas')
  off.width = width
  off.height = height
  const c = off.getContext('2d', { willReadFrequently: true })
  if (!c) return { points: [], step: 1, size: 0 }

  // Keep in sync with the splash stylesheet's font-size: the form the motes
  // build has to match the space the layout expects. The family is read from
  // --font-display so changing it in one place changes the wordmark too; a
  // hard-coded stack here silently ignored that variable.
  const size = Math.min(width / (TEXT.length * 0.62), height * 0.20)
  const display = getComputedStyle(document.documentElement)
    .getPropertyValue('--font-display')
    .trim() || '-apple-system, BlinkMacSystemFont, system-ui, sans-serif'
  c.font = `800 ${size}px ${display}`
  c.textAlign = 'center'
  c.textBaseline = 'middle'
  c.fillStyle = '#fff'
  c.fillText(TEXT, width / 2, height / 2)

  const { data } = c.getImageData(0, 0, width, height)
  const opaque = (x, y) => data[(y * width + x) * 4 + 3] > 128

  // First pass: a coarse grid, only to learn how much glyph there is. A single
  // pass would either overflow the budget on a large window or leave the word
  // half-empty on a small one; scaling the step by the measured count fixes
  // both without resampling the text itself.
  const probe = Math.max(2, Math.round(size / 110))
  let count = 0
  for (let y = 0; y < height; y += probe) {
    for (let x = 0; x < width; x += probe) {
      if (opaque(x, y)) count += 1
    }
  }
  const scale = count > 0 ? Math.sqrt(count / TARGET_POINTS) : 1
  const step = Math.max(1, Math.round(probe * Math.max(0.4, scale)))

  // Second pass: the real sample, with jitter so the settled word reads as light
  // rather than as a scan-line grid.
  const points = []
  const jitter = step * 0.55
  for (let y = 0; y < height; y += step) {
    for (let x = 0; x < width; x += step) {
      if (!opaque(x, y)) continue
      points.push(x + (Math.random() - 0.5) * jitter, y + (Math.random() - 0.5) * jitter)
    }
  }
  return { points, step, size }
}

export default function SplashAnimation () {
  const canvasRef = useRef(null)
  const sentRef = useRef(false)
  // Only the exit phase is stateful now: with no text layer there is nothing to
  // reveal part-way through.
  const [exiting, setExiting] = useState(false)

  useEffect(() => {
    const done = () => {
      if (sentRef.current) return
      sentRef.current = true
      try {
        window.__TAURI__.core.invoke('splash_done')
      } catch (_) {}
    }
    const finish = setTimeout(done, TOTAL)
    const guard = setTimeout(done, TOTAL + 900)
    return () => {
      clearTimeout(finish)
      clearTimeout(guard)
    }
  }, [])

  useEffect(() => {
    // Exit at the same point in the film as before (4400/5400 of the way in).
    const b = setTimeout(() => setExiting(true), TOTAL * 0.815)
    return () => clearTimeout(b)
  }, [])

  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas) return
    const ctx = canvas.getContext('2d')
    if (!ctx) return

    const dpr = Math.min(window.devicePixelRatio || 1, 2)
    let raf = 0
    let width = 0
    let height = 0
    let stats = null
    let motes = []
    let ambient = []

    /* Reused across frames: allocating per frame at this count is what makes
       the intro stutter, and the buckets are what keep the draw calls few. */
    const bucketX = []
    const bucketY = []
    const bucketS = []
    for (let i = 0; i < HUE_BINS * ALPHA_BINS; i += 1) {
      bucketX.push([])
      bucketY.push([])
      bucketS.push([])
    }

    const build = () => {

      width = Math.floor(window.innerWidth * dpr)
      height = Math.floor(window.innerHeight * dpr)
      canvas.width = width
      canvas.height = height
      canvas.style.width = window.innerWidth + 'px'
      canvas.style.height = window.innerHeight + 'px'

      const { points, step, size } = measureWord(width, height)
      // Reported on the first frame, not here: `__TAURI__` is injected by the
      // IPC bootstrap and is not guaranteed to exist while the effect body runs.
      stats = { points: points.length / 2, step, size }

      const reach = Math.hypot(width, height) / 2
      const total = points.length / 2

      // One mote per sample point: the word has no other layer, so any point
      // left uncovered is a hole in the letterform.
      motes = new Array(total)
      for (let i = 0; i < total; i += 1) {
        const tx = points[i * 2]
        const ty = points[i * 2 + 1]

        // Origin: a wide disc, biased to the screen edges so the gathering reads
        // as the whole field collapsing inward rather than a local swirl.
        const a = Math.random() * Math.PI * 2
        const r = reach * (0.45 + Math.random() * 1.35)

        motes[i] = {
          a,
          r,
          tx,
          ty,
          // Colour follows horizontal position, so the finished word is a
          // rainbow rather than one tint: cyan on the left, gold on the right.
          hue: HUE_LO + (tx / width) * HUE_SPAN,
          // Staggered arrival keeps the shape resolving rather than snapping.
          delay: Math.random() * 380,
          size: (Math.random() * 1.05 + 0.85) * dpr,
          swirl: (Math.random() - 0.5) * 2.2,
        }
      }

      ambient = Array.from({ length: AMBIENT }, () => ({
        x: Math.random() * width,
        y: Math.random() * height,
        vx: (Math.random() - 0.5) * 0.02,
        vy: (Math.random() - 0.5) * 0.02,
        size: (Math.random() * 1.2 + 0.6) * dpr,
        hue: HUE_LO + Math.random() * HUE_SPAN,
        phase: Math.random() * Math.PI * 2,
      }))

      for (let i = 0; i < bucketX.length; i += 1) {
        bucketX[i].length = 0
        bucketY[i].length = 0
        bucketS[i].length = 0
      }
    }
    // Canvas ignores font-display, so an early raster would carve the fallback's
    // letterforms and the motes would assemble into the wrong word for the whole
    // run. Wait for the real face, but never block the animation on it: a font
    // that fails to load must still leave a launch that completes.
    const started = performance.now()
    const ready = Promise.race([
      document.fonts.load(`800 64px ${getComputedStyle(document.documentElement)
        .getPropertyValue('--font-display').trim()}`).catch(() => {}),
      // Capped in proportion to the film: on a 3s launch a 400ms stall before
      // anything is drawn would be a visible pause at the start.
      new Promise((r) => setTimeout(r, TOTAL * 0.074)),
    ])
    ready.then(build)

    const frame = (now) => {
      if (stats) {
        const s0 = stats
        stats = null
        try {
          window.__TAURI__.core.invoke('splash_stats', s0)
        } catch (_) {}
      }
      const elapsed = now - started
      const t = clamp01(elapsed / TOTAL)
      ctx.clearRect(0, 0, width, height)

      const inA = clamp01(t / 0.10)
      const outA = t < 0.81 ? 1 : clamp01(1 - (t - 0.81) / 0.19)
      const globalA = inA * outA

      ctx.globalCompositeOperation = 'lighter'

      /* --- ambient motes: they keep the air full, then clear out of the way --- */
      // Ambient light clears between 1400ms and 2600ms of the original 5400ms
      // film; both ends scale with TOTAL so the overlap with the letters is
      // unchanged when the film is shortened.
      const airA = clamp01(1 - (elapsed - TOTAL * 0.259) / (TOTAL * 0.222))
      if (airA > 0.01) {
        for (const m of ambient) {
          const x = m.x + Math.sin(elapsed * 0.00025 + m.phase) * 18 + m.vx * elapsed
          const y = m.y + Math.cos(elapsed * 0.00021 + m.phase) * 18 + m.vy * elapsed
          ctx.fillStyle = `hsla(${m.hue} 90% 70% / ${0.22 * airA * globalA})`
          ctx.fillRect(x, y, m.size, m.size)
        }
      }

      /* --- the word: every mote carries one sample point --- */
      for (let i = 0; i < bucketX.length; i += 1) {
        bucketX[i].length = 0
        bucketY[i].length = 0
        bucketS[i].length = 0
      }

      // Settled motes stay near full brightness: with no crisp text beneath,
      // dimming them would take the word apart just as it becomes readable.
      const settleDim = 0.12
      for (let i = 0; i < motes.length; i += 1) {
        const m = motes[i]
        // Gathering starts at 420ms and takes 2250ms of the original film.
        const local = clamp01((elapsed - TOTAL * 0.0778 - m.delay) / (TOTAL * 0.4167))
        const eased = easeOutCubic(local)

        // Spiral inward: the angle keeps turning while the radius collapses, so
        // the motion curves instead of sliding straight at the target.
        const a = m.a + m.swirl * (1 - eased) * 1.6
        const r = m.r * (1 - eased)
        const x = m.tx + Math.cos(a) * r
        const y = m.ty + Math.sin(a) * r * 0.92

        const alpha = (0.34 + eased * 0.66) * (1 - eased * settleDim) * globalA
        if (alpha <= 0.004) continue

        const hb = Math.min(HUE_BINS - 1, ((m.hue - HUE_LO) / HUE_SPAN * HUE_BINS) | 0)
        const ab = Math.min(ALPHA_BINS - 1, (alpha * ALPHA_BINS) | 0)
        const k = hb * ALPHA_BINS + ab
        bucketX[k].push(x)
        bucketY[k].push(y)
        bucketS[k].push(m.size)
      }

      // One fill per colour/opacity bucket instead of one per mote: at this
      // count, per-mote fillStyle changes dominate the frame.
      for (let k = 0; k < bucketX.length; k += 1) {
        const xs = bucketX[k]
        if (xs.length === 0) continue
        const ys = bucketY[k]
        const ss = bucketS[k]
        const hb = (k / ALPHA_BINS) | 0
        const ab = k % ALPHA_BINS
        const hue = HUE_LO + ((hb + 0.5) / HUE_BINS) * HUE_SPAN
        const alpha = (ab + 0.5) / ALPHA_BINS
        ctx.fillStyle = `hsla(${hue} 95% 68% / ${alpha})`
        ctx.beginPath()
        for (let i = 0; i < xs.length; i += 1) {
          const s = ss[i]
          ctx.rect(xs[i] - s / 2, ys[i] - s / 2, s, s)
        }
        ctx.fill()
      }

      ctx.globalCompositeOperation = 'source-over'

      if (elapsed < TOTAL) raf = requestAnimationFrame(frame)
    }

    raf = requestAnimationFrame(frame)
    window.addEventListener('resize', build)
    return () => {
      cancelAnimationFrame(raf)
      window.removeEventListener('resize', build)
    }
  }, [])

  return (
    <div className={`splash${exiting ? ' is-exiting' : ''}`}>
      <div className="splash-grain" />
      <canvas ref={canvasRef} className="splash-dust" />
    </div>
  )
}
