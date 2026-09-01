// Connects to /ws/events and maintains a bounded in-memory event log.
// Auto-reconnects every 3 seconds on close/error.

import { useEffect, useRef, useState, useCallback } from 'react'

export interface PanelEvent {
  type: string
  ts?: string
  [key: string]: unknown
}

export function useWebSocket(maxEvents = 50) {
  const [events, setEvents] = useState<PanelEvent[]>([])
  const [connected, setConnected] = useState(false)
  const wsRef = useRef<WebSocket | null>(null)
  const reconnectTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  // Track mount state to prevent reconnect after unmount
  const mounted = useRef(true)

  const connect = useCallback(() => {
    if (!mounted.current) return
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const authToken = localStorage.getItem('panel_token') ?? ''
    const url = `${protocol}//${window.location.host}/ws/events`
    // Token rides in Sec-WebSocket-Protocol (not ?auth=) so it never lands
    // in access logs or browser history (#653).
    let ws: WebSocket
    try {
      ws = authToken ? new WebSocket(url, [authToken]) : new WebSocket(url)
    } catch {
      // Non-RFC7230 chars in the token make the constructor throw before any
      // event fires; schedule the usual 3s retry so the loop survives (#653).
      reconnectTimer.current = setTimeout(connect, 3_000)
      return
    }
    wsRef.current = ws

    ws.onopen = () => {
      if (mounted.current) setConnected(true)
    }

    ws.onclose = () => {
      if (!mounted.current) return
      setConnected(false)
      reconnectTimer.current = setTimeout(connect, 3_000)
    }

    ws.onerror = () => {
      ws.close()
    }

    ws.onmessage = (msg) => {
      if (!mounted.current) return
      try {
        const event = JSON.parse(msg.data as string) as PanelEvent
        // Inject a client-side timestamp if the server omits one
        if (!event.ts) event.ts = new Date().toISOString()
        setEvents((prev) => [event, ...prev].slice(0, maxEvents))
      } catch {
        // Ignore malformed frames
      }
    }
  }, [maxEvents])

  useEffect(() => {
    mounted.current = true
    connect()
    return () => {
      mounted.current = false
      if (reconnectTimer.current) clearTimeout(reconnectTimer.current)
      wsRef.current?.close()
    }
  }, [connect])

  return { events, connected }
}
