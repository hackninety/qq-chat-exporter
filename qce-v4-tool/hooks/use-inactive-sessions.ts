import { useCallback, useEffect, useRef, useState } from "react"
import type { InactiveSessionsResponse } from "@/types/api"
import { useApi } from "./use-api"

export function useInactiveSessions(accountUin?: string) {
  const [data, setData] = useState<InactiveSessionsResponse | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const { apiCall } = useApi()
  const accountUinRef = useRef(accountUin)

  const loadInactiveSessions = useCallback(async () => {
    const requestedAccountUin = accountUinRef.current
    if (!requestedAccountUin) return null
    setLoading(true)
    setError(null)
    try {
      const response = await apiCall<InactiveSessionsResponse>(
        "/api/inactive-sessions?limit=2000",
      )
      if (!response.success || !response.data) {
        throw new Error(response.error?.message || "获取已删除/退出会话失败")
      }
      if (accountUinRef.current !== requestedAccountUin) return null
      setData(response.data)
      return response.data
    } catch (loadError) {
      const message = loadError instanceof Error ? loadError.message : "获取已删除/退出会话失败"
      if (accountUinRef.current === requestedAccountUin) setError(message)
      return null
    } finally {
      if (accountUinRef.current === requestedAccountUin) setLoading(false)
    }
  }, [apiCall])

  useEffect(() => {
    accountUinRef.current = accountUin
    setData(null)
    setError(null)
    setLoading(false)
  }, [accountUin])

  return {
    data,
    loading,
    error,
    loadInactiveSessions,
  }
}
