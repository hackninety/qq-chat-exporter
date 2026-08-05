import { useCallback, useState } from "react"
import type { InactiveSessionsResponse } from "@/types/api"
import { useApi } from "./use-api"

export function useInactiveSessions() {
  const [data, setData] = useState<InactiveSessionsResponse | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const { apiCall } = useApi()

  const loadInactiveSessions = useCallback(async () => {
    setLoading(true)
    setError(null)
    try {
      const response = await apiCall<InactiveSessionsResponse>(
        "/api/inactive-sessions?limit=2000",
      )
      if (!response.success || !response.data) {
        throw new Error(response.error?.message || "获取已删除/退出会话失败")
      }
      setData(response.data)
      return response.data
    } catch (loadError) {
      const message = loadError instanceof Error ? loadError.message : "获取已删除/退出会话失败"
      setError(message)
      return null
    } finally {
      setLoading(false)
    }
  }, [apiCall])

  return {
    data,
    loading,
    error,
    loadInactiveSessions,
  }
}
