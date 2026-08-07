import { useState, useCallback } from "react"
import type { AccountLogoutResult, SystemInfo } from "@/types/api"
import { useApi } from "./use-api"

export function useSystemInfo() {
  const [systemInfo, setSystemInfo] = useState<SystemInfo | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const { apiCall } = useApi()

  const loadSystemInfo = useCallback(async () => {
    try {
      setLoading(true)
      setError(null)
      const response = await apiCall<SystemInfo>("/api/system/info")
      if (response.success && response.data) {
        setSystemInfo(response.data)
      }
    } catch (err) {
      const errorMessage = `加载系统信息失败: ${err instanceof Error ? err.message : "未知错误"}`
      setError(errorMessage)
      console.error("[QCE] System info error:", err)
    } finally {
      setLoading(false)
    }
  }, [apiCall])

  const refreshSystemInfo = useCallback(() => {
    loadSystemInfo()
  }, [loadSystemInfo])

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
