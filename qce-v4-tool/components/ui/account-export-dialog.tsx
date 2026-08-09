"use client"

import { useEffect, useMemo, useState } from "react"
import { AlertTriangle, Archive, Database, Loader2 } from "lucide-react"

import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import type {
  APIResponse,
  AccountExportPreview,
  ChatBackupImport,
} from "@/types/api"

interface AccountExportDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  imports: ChatBackupImport[]
  importsLoading: boolean
  account?: { uin?: string; nick?: string }
  onStarted?: (taskId: string) => void
}

export function AccountExportDialog({
  open,
  onOpenChange,
  imports,
  importsLoading,
  account,
  onStarted,
}: AccountExportDialogProps) {
  const [backupImportId, setBackupImportId] = useState("")
  const [debugExport, setDebugExport] = useState(true)
  const [resume, setResume] = useState(true)
  const [preview, setPreview] = useState<AccountExportPreview | null>(null)
  const [previewLoading, setPreviewLoading] = useState(false)
  const [creating, setCreating] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!open) return
    setDebugExport(true)
    if (!backupImportId || !imports.some((item) => item.id === backupImportId)) {
      setBackupImportId(imports[0]?.id ?? "")
    }
  }, [backupImportId, imports, open])

  useEffect(() => {
    if (!open || !backupImportId) {
      setPreview(null)
      return
    }
    const controller = new AbortController()
    setPreviewLoading(true)
    setError(null)
    fetch("/api/account-exports/preview", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ backupImportId }),
      signal: controller.signal,
    })
      .then(async (response) => {
        const result = await response.json() as APIResponse<AccountExportPreview>
        if (!response.ok || !result.success || !result.data) {
          throw new Error(result.error?.message || "全账号归档预检失败")
        }
        setPreview(result.data)
        setResume(result.data.resumeAvailable !== false)
      })
      .catch((previewError) => {
        if (previewError instanceof DOMException && previewError.name === "AbortError") return
        setPreview(null)
        setError(previewError instanceof Error ? previewError.message : "全账号归档预检失败")
      })
      .finally(() => setPreviewLoading(false))
    return () => controller.abort()
  }, [backupImportId, open])

  const selectedBackup = useMemo(
    () => imports.find((item) => item.id === backupImportId),
    [backupImportId, imports],
  )

  const createExport = async () => {
    if (!backupImportId || creating) return
    setCreating(true)
    setError(null)
    try {
      const response = await fetch("/api/account-exports", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ backupImportId, debugExport, resume }),
      })
      const result = await response.json() as APIResponse<{ taskId: string }>
      if (!response.ok || !result.success || !result.data?.taskId) {
        throw new Error(result.error?.message || "创建全账号归档任务失败")
      }
      onStarted?.(result.data.taskId)
      onOpenChange(false)
    } catch (createError) {
      setError(createError instanceof Error ? createError.message : "创建全账号归档任务失败")
    } finally {
      setCreating(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-2xl p-0 overflow-hidden">
        <DialogHeader className="border-b px-6 py-5 text-left">
          <DialogTitle className="flex items-center gap-2 text-lg">
            <Archive className="h-5 w-5" />
            创建导出任务
          </DialogTitle>
          <DialogDescription>
            将历史数据库、当前 QQ 会话和可访问资源合并为可离线检索的 QCE Archive v2。
          </DialogDescription>
        </DialogHeader>

        <div className="max-h-[70vh] space-y-5 overflow-y-auto px-6 py-5">
          <div className="rounded-xl bg-muted/40 p-4">
            <p className="text-sm font-medium">当前账号</p>
            <p className="mt-1 text-sm text-muted-foreground">
              {account?.nick || preview?.account.name || "QQ 账号"}
              {(account?.uin || preview?.account.uin) && `（${account?.uin || preview?.account.uin}）`}
            </p>
          </div>

          <label className="block space-y-2">
            <span className="text-sm font-medium">历史主数据库</span>
            <select
              className="h-10 w-full rounded-lg border bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-ring"
              value={backupImportId}
              onChange={(event) => setBackupImportId(event.target.value)}
              disabled={importsLoading || creating}
            >
              {imports.map((item) => (
                <option key={item.id} value={item.id}>
                  {item.fileName} · {item.sessionCount.toLocaleString()} 个会话 · {item.messageCount.toLocaleString()} 条
                </option>
              ))}
            </select>
          </label>

          {!importsLoading && imports.length === 0 && (
            <div className="rounded-xl border border-amber-200 bg-amber-50 p-4 text-sm text-amber-800 dark:border-amber-900 dark:bg-amber-950/30 dark:text-amber-200">
              请先在“已删除/退出”页面导入并解密 nt_msg.db，再回来创建全账号归档。
            </div>
          )}

          {previewLoading && (
            <div className="flex items-center gap-2 rounded-xl border p-4 text-sm text-muted-foreground">
              <Loader2 className="h-4 w-4 animate-spin" /> 正在预检账号、会话和关系状态…
            </div>
          )}

          {preview && !previewLoading && (
            <div className="space-y-4 rounded-xl border p-4">
              <div className="flex items-center gap-2">
                <Database className="h-4 w-4 text-muted-foreground" />
                <p className="text-sm font-medium">预检结果</p>
              </div>
              <div className="grid grid-cols-2 gap-3 text-sm sm:grid-cols-3">
                <Metric label="全部会话" value={preview.counts.total} />
                <Metric label="好友" value={preview.counts.friend} />
                <Metric label="已删除/非好友" value={preview.counts.nonFriend} />
                <Metric label="群聊" value={preview.counts.group} />
                <Metric label="已退出/不可用群" value={preview.counts.unavailableGroup} />
                <Metric label="其他" value={preview.counts.other} />
              </div>
              <div className="space-y-1 text-xs leading-relaxed text-muted-foreground">
                {preview.fixedIncludes.map((item) => <p key={item}>· {item}</p>)}
              </div>
              <p className="text-xs leading-relaxed text-amber-700 dark:text-amber-300">
                <AlertTriangle className="mr-1 inline h-3.5 w-3.5" />
                {preview.notice} 全量资源可能占用较多磁盘并持续较长时间；单项失败会记录警告，不会中止整个归档。
              </p>
            </div>
          )}

          {preview?.resumeAvailable && (
            <label className="flex items-start gap-3 rounded-xl border border-blue-200 bg-blue-50/60 p-4 dark:border-blue-900 dark:bg-blue-950/20">
              <Checkbox
                checked={resume}
                onCheckedChange={(checked) => setResume(checked === true)}
                disabled={creating}
              />
              <span className="space-y-1">
                <span className="block text-sm font-medium">继续上次未完成的导出</span>
                <span className="block text-xs leading-relaxed text-muted-foreground">
                  已保存 {preview.completedCheckpointCount?.toLocaleString() ?? 0} 个会话断点；继续后会跳过这些会话的读取、解析与媒体下载。
                </span>
              </span>
            </label>
          )}

          <label className="flex items-start gap-3 rounded-xl border p-4">
            <Checkbox
              checked={debugExport}
              onCheckedChange={(checked) => setDebugExport(checked === true)}
              disabled={creating}
            />
            <span className="space-y-1">
              <span className="block text-sm font-medium">同时生成 .debug 旁路目录</span>
              <span className="block text-xs leading-relaxed text-muted-foreground">
                保存逐会话原始/解析消息、资源事件和缺失摘要。默认开启，便于未来发现问题时追溯归档来源。
              </span>
            </span>
          </label>

          {selectedBackup && (
            <p className="text-xs text-muted-foreground">
              历史主源：{selectedBackup.fileName}（{selectedBackup.format}，{selectedBackup.messageCount.toLocaleString()} 条）
            </p>
          )}
          {error && <p className="text-sm text-destructive">{error}</p>}
        </div>

        <div className="flex items-center justify-end gap-2 border-t px-6 py-4">
          <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={creating}>取消</Button>
          <Button
            onClick={createExport}
            disabled={!preview || !backupImportId || creating || previewLoading}
          >
            {creating && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
            创建导出任务
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  )
}

function Metric({ label, value }: { label: string; value: number }) {
  return (
    <div className="rounded-lg bg-muted/40 px-3 py-2">
      <p className="text-lg font-semibold tabular-nums">{value.toLocaleString()}</p>
      <p className="text-xs text-muted-foreground">{label}</p>
    </div>
  )
}
