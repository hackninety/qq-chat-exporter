"use client"

import { useCallback, useEffect, useRef, useState } from "react"
import type {
  APIResponse,
  ChatBackupImport,
  ChatBackupKeyDetection,
  ChatBackupsResponse,
} from "@/types/api"

function responseError(result: APIResponse<unknown>, fallback: string): string {
  return result.error?.message || fallback
}

export function useChatBackups(accountUin?: string) {
  const [imports, setImports] = useState<ChatBackupImport[]>([])
  const [loading, setLoading] = useState(false)
  const [importing, setImporting] = useState(false)
  const [detectingKey, setDetectingKey] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [loaded, setLoaded] = useState(false)
  const keyDetectionCount = useRef(0)
  const accountUinRef = useRef(accountUin)

  const loadBackups = useCallback(async () => {
    const requestedAccountUin = accountUinRef.current
    if (!requestedAccountUin) {
      setImports([])
      setLoaded(false)
      return
    }
    setLoading(true)
    setError(null)
    try {
      const importsResponse = await fetch("/api/chat-backups")
      const importsResult = await importsResponse.json() as APIResponse<ChatBackupsResponse>
      if (!importsResponse.ok || !importsResult.success || !importsResult.data) {
        throw new Error(responseError(importsResult, "读取已导入备份失败"))
      }
      if (accountUinRef.current === requestedAccountUin) {
        setImports(importsResult.data.imports || [])
        setLoaded(true)
      }
    } catch (loadError) {
      if (accountUinRef.current === requestedAccountUin) {
        setError(loadError instanceof Error ? loadError.message : "读取聊天记录备份失败")
      }
    } finally {
      if (accountUinRef.current === requestedAccountUin) setLoading(false)
    }
  }, [])

  useEffect(() => {
    accountUinRef.current = accountUin
    setImports([])
    setLoaded(false)
    setError(null)
    setLoading(Boolean(accountUin))
    if (accountUin) void loadBackups()
  }, [accountUin, loadBackups])

  const importFile = useCallback(async (file: File, key?: string) => {
    setImporting(true)
    setError(null)
    try {
      const form = new FormData()
      if (key?.trim()) form.append("key", key.trim())
      form.append("file", file, file.name)
      const response = await fetch("/api/chat-backups/upload", {
        method: "POST",
        body: form,
      })
      const result = await response.json() as APIResponse<{ import: ChatBackupImport }>
      if (!response.ok || !result.success) {
        throw new Error(responseError(result, "导入聊天记录备份失败"))
      }
      await loadBackups()
      return result.data?.import ?? null
    } catch (importError) {
      const message = importError instanceof Error ? importError.message : "导入聊天记录备份失败"
      setError(message)
      throw new Error(message)
    } finally {
      setImporting(false)
    }
  }, [loadBackups])

  const detectKey = useCallback(async (file: File): Promise<ChatBackupKeyDetection> => {
    keyDetectionCount.current += 1
    setDetectingKey(true)
    try {
      const form = new FormData()
      const sample = file.slice(0, 8 * 1024 * 1024, "application/octet-stream")
      form.append("originalSize", String(file.size))
      form.append("file", sample, file.name)
      const response = await fetch("/api/chat-backups/detect-key", {
        method: "POST",
        body: form,
        cache: "no-store",
      })
      const result = await response.json() as APIResponse<ChatBackupKeyDetection>
      if (!response.ok || !result.success || !result.data) {
        throw new Error(responseError(result, "自动检测 NTQQ 数据库密钥失败"))
      }
      return result.data
    } finally {
      keyDetectionCount.current -= 1
      if (keyDetectionCount.current === 0) setDetectingKey(false)
    }
  }, [])

  return {
    imports,
    loading,
    importing,
    detectingKey,
    error,
    loaded,
    loadBackups,
    detectKey,
    importFile,
  }
}
