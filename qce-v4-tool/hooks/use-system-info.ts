import { useState, useCallback, useRef } from "react"
import type { AccountLogoutResult, SystemInfo } from "@/types/api"
import { useApi } from "./use-api"

export function useSystemInfo() {
  const [systemInfo, setSystemInfo] = useState<SystemInfo | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const { apiCall } = useApi()
  const requestSequence = useRef(0)
  const loadingRequestSequence = useRef(0)

  const requestSystemInfo = useCallback(async (silent: boolean) => {
    const requestId = requestSequence.current + 1
    requestSequence.current = requestId
    try {
      if (!silent) {
        loadingRequestSequence.current = requestId
        setLoading(true)
        setError(null)
      }
      const response = await apiCall<SystemInfo>("/api/system/info")
      if (requestSequence.current === requestId && response.success && response.data) {
        setSystemInfo(response.data)
      }
    } catch (err) {
      const errorMessage = `加载系统信息失败: ${err instanceof Error ? err.message : "未知错误"}`
      if (!silent && requestSequence.current === requestId) setError(errorMessage)
      console.error("[QCE] System info error:", err)
    } finally {
      if (!silent && loadingRequestSequence.current === requestId) setLoading(false)
    }
  }, [apiCall])

  const loadSystemInfo = useCallback(async () => {
    await requestSystemInfo(false)
  }, [requestSystemInfo])

  const refreshSystemInfo = useCallback(async () => {
    await requestSystemInfo(true)
  }, [requestSystemInfo])

  const logoutAccount = useCallback(async () => {
    const response = await apiCall<AccountLogoutResult>("/api/system/logout", {
      method: "POST",
    })
    if (!response.success || !response.data) {
      throw new Error(response.error?.message || "注销失败")
    }
    setSystemInfo((current) => current ? {
      ...current,
      quickLogin: {
        enabled: false,
        account: "",
        credentialOwner: "qqnt",
      },
    } : current)
    return response.data
  }, [apiCall])

  return {
    systemInfo,
    loading,
    error,
    loadSystemInfo,
    refreshSystemInfo,
    logoutAccount,
  }
}
