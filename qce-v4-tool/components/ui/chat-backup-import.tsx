"use client"

import { useRef, useState } from "react"
import {
  AlertCircle,
  Archive,
  CheckCircle2,
  Database,
  FileArchive,
  Info,
  KeyRound,
  RefreshCw,
  Upload,
} from "lucide-react"
import type { ChatBackupImport, ChatBackupKeyDetection } from "@/types/api"
import { Button } from "./button"
import { Input } from "./input"
import { Loader } from "./loader"

interface ChatBackupImportProps {
  accountUin?: string
  imports: ChatBackupImport[]
  loading: boolean
  importing: boolean
  detectingKey: boolean
  error: string | null
  onRefresh: () => void
  onDetectKey: (file: File) => Promise<ChatBackupKeyDetection>
  onImportFile: (file: File, key?: string) => Promise<ChatBackupImport | null>
  onExportAll: () => void
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KiB`
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MiB`
  return `${(bytes / 1024 ** 3).toFixed(2)} GiB`
}

export function ChatBackupImportSection({
  accountUin,
  imports,
  loading,
  importing,
  detectingKey,
  error,
  onRefresh,
  onDetectKey,
  onImportFile,
  onExportAll,
}: ChatBackupImportProps) {
  const [showImporter, setShowImporter] = useState(false)
  const [selectedFile, setSelectedFile] = useState<File | null>(null)
  const [key, setKey] = useState("")
  const [keyNotice, setKeyNotice] = useState<{ kind: 'success' | 'info' | 'error', message: string } | null>(null)
  const inputRef = useRef<HTMLInputElement>(null)
  const detectionRequestRef = useRef(0)

  const handleImport = async () => {
    if (!selectedFile) return
    await onImportFile(selectedFile, key).then((result) => {
      if (result) {
        detectionRequestRef.current += 1
        setSelectedFile(null)
        setKey("")
        setKeyNotice(null)
        if (inputRef.current) inputRef.current.value = ""
        setShowImporter(false)
      }
    }).catch(() => undefined)
  }

  const handleSelectedFile = async (file: File | null) => {
    const requestId = detectionRequestRef.current + 1
    detectionRequestRef.current = requestId
    setSelectedFile(file)
    setKey("")
    setKeyNotice(null)
    if (!file) return

    try {
      const detection = await onDetectKey(file)
      if (detectionRequestRef.current !== requestId) return
      if (!detection.required) {
        setKeyNotice({ kind: 'info', message: '检测到明文 SQLite 数据库，无需填写密钥。' })
      } else if (detection.detected && detection.key) {
        setKey(detection.key)
        setKeyNotice({ kind: 'success', message: '已从本机登录中的 QQ 自动检测并填入数据库密钥。' })
      } else {
        setKeyNotice({ kind: 'error', message: '未能自动检测密钥，可以继续手动填写。' })
      }
    } catch (detectionError) {
      if (detectionRequestRef.current !== requestId) return
      const message = detectionError instanceof Error ? detectionError.message : '自动检测密钥失败'
      setKeyNotice({ kind: 'error', message: `${message}；可以继续手动填写。` })
    }
  }

  const handleKeyChange = (value: string) => {
    detectionRequestRef.current += 1
    setKey(value)
    setKeyNotice(null)
  }

  const handleCancelImport = () => {
    detectionRequestRef.current += 1
    setShowImporter(false)
  }

  return (
    <section className="space-y-4 border-t border-black/[0.05] pt-6 dark:border-white/[0.07]">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <h2 className="text-[15px] font-semibold text-foreground">导入的聊天记录备份</h2>
          <p className="mt-1 text-xs leading-relaxed text-muted-foreground/70">
            当前 QQ {accountUin || "未登录"}；这里只显示并使用该账号导入的历史私聊和群聊。
          </p>
        </div>
        <div className="flex items-center gap-2">
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="h-8 w-8 rounded-full"
            onClick={onRefresh}
            disabled={loading || importing}
            aria-label="刷新导入的聊天记录"
          >
            <RefreshCw className={`h-3.5 w-3.5 ${loading ? 'animate-spin' : ''}`} />
          </Button>
          <Button
            type="button"
            size="sm"
            className="h-8 rounded-full px-3 text-[12px]"
            onClick={() => setShowImporter((visible) => !visible)}
          >
            <Upload className="mr-1.5 h-3.5 w-3.5" />
            导入备份
          </Button>
        </div>
      </div>

      {showImporter && (
        <div className="space-y-4 rounded-2xl bg-black/[0.025] p-4 dark:bg-white/[0.035]">
          <label className="flex cursor-pointer items-center gap-3 rounded-xl border border-dashed border-black/10 bg-background/60 px-4 py-4 transition-colors hover:border-black/20 dark:border-white/15 dark:hover:border-white/25">
            <FileArchive className="h-5 w-5 shrink-0 text-muted-foreground/60" />
            <span className="min-w-0 flex-1">
              <span className="block truncate text-sm text-foreground">
                {selectedFile?.name || '选择 .db、.sqlite 或 .bak 文件'}
              </span>
              <span className="mt-0.5 block text-[11px] text-muted-foreground/60">
                {selectedFile ? formatSize(selectedFile.size) : '大文件会流式上传，最大 8 GiB'}
              </span>
            </span>
            <input
              ref={inputRef}
              type="file"
              accept=".db,.sqlite,.bak"
              className="sr-only"
              onChange={(event) => void handleSelectedFile(event.target.files?.[0] || null)}
            />
          </label>

          <div className="relative">
            <KeyRound className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground/55" />
            <Input
              type="password"
              value={key}
              onChange={(event) => handleKeyChange(event.target.value)}
              placeholder="NTQQ 数据库密钥（明文导出库可留空）"
              autoComplete="off"
              className="h-9 rounded-full border-0 bg-background pl-9 pr-24 text-[13px]"
            />
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="absolute right-1 top-1/2 h-7 -translate-y-1/2 rounded-full px-2.5 text-[11px]"
              disabled={!selectedFile || detectingKey}
              onClick={() => selectedFile && void handleSelectedFile(selectedFile)}
            >
              {detectingKey ? <Loader size={12} className="mr-1" /> : <KeyRound className="mr-1 h-3 w-3" />}
              {detectingKey ? '检测中' : '自动检测'}
            </Button>
          </div>

          {(detectingKey || keyNotice) && (
            <div className={`flex items-start gap-2 text-[11px] leading-relaxed ${keyNotice?.kind === 'error' ? 'text-amber-700 dark:text-amber-300' : keyNotice?.kind === 'success' ? 'text-emerald-700 dark:text-emerald-300' : 'text-muted-foreground/70'}`}>
              {detectingKey ? (
                <Loader size={13} className="mt-0.5 shrink-0" />
              ) : keyNotice?.kind === 'success' ? (
                <CheckCircle2 className="mt-0.5 h-3.5 w-3.5 shrink-0" />
              ) : keyNotice?.kind === 'error' ? (
                <AlertCircle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
              ) : (
                <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" />
              )}
              <span>{detectingKey ? '正在只读扫描本机已登录的 QQ，并用所选数据库验证候选密钥…' : keyNotice?.message}</span>
            </div>
          )}

          <div className="flex items-start gap-2 text-[11px] leading-relaxed text-muted-foreground/65">
            <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <p>
              自动检测仅以读取权限扫描本机已登录 QQ 的进程内存，并用所选数据库验证候选；不会注入、调试、暂停或修改 QQ。QCE 绝不写回源数据库，密钥仅用于本次解密且不会保存，也可随时手动覆盖。传统 PCQQ 加密 .bak 尚无完整公开离线解密方案。
            </p>
          </div>

          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" size="sm" className="rounded-full" onClick={handleCancelImport}>
              取消
            </Button>
            <Button
              type="button"
              size="sm"
              className="rounded-full"
              disabled={importing || detectingKey || !selectedFile}
              onClick={handleImport}
            >
              {importing ? <Loader size={14} className="mr-1.5" /> : <Database className="mr-1.5 h-3.5 w-3.5" />}
              {importing ? '正在解析…' : '开始导入'}
            </Button>
          </div>
        </div>
      )}

      {error && (
        <div className="flex items-start gap-2 rounded-xl bg-red-50 px-3 py-2.5 text-xs leading-relaxed text-red-700 dark:bg-red-950/30 dark:text-red-300">
          <AlertCircle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <span>{error}</span>
        </div>
      )}

      {imports.length > 0 && (
        <div className="flex flex-wrap gap-2">
          {imports.map((item) => (
            <span key={item.id} className="rounded-full bg-black/[0.035] px-3 py-1.5 text-[11px] text-muted-foreground dark:bg-white/[0.05]">
              {item.fileName} · {item.sessionCount} 个会话 · {item.messageCount.toLocaleString()} 条
            </span>
          ))}
        </div>
      )}

      <div className="flex flex-col gap-3 rounded-2xl bg-black/[0.025] p-4 dark:bg-white/[0.035] sm:flex-row sm:items-center sm:justify-between">
        <div className="min-w-0">
          <p className="text-sm font-medium text-foreground">全账号完整归档</p>
          <p className="mt-1 text-[11px] leading-relaxed text-muted-foreground/65">
            以已导入的 nt_msg.db 为历史主源，并补齐当前会话、头像、消息媒体、表情、群文件和群相册，生成可离线检索的 .qcearchive。
          </p>
        </div>
        <Button
          type="button"
          size="sm"
          variant="outline"
          data-testid="inactive-account-export-button"
          className="h-8 shrink-0 rounded-full px-3 text-[12px]"
          disabled={loading || importing || imports.length === 0}
          onClick={onExportAll}
        >
          <Archive className="mr-1.5 h-3.5 w-3.5" />
          导出所有聊天记录
        </Button>
      </div>

      {loading && imports.length === 0 ? (
        <div className="flex items-center justify-center py-10 text-xs text-muted-foreground">
          <Loader size={18} className="mr-2" />读取导入记录…
        </div>
      ) : imports.length === 0 ? (
        <div className="flex flex-col items-center justify-center rounded-xl py-10 text-center">
          <FileArchive className="mb-2 h-8 w-8 text-muted-foreground/20" />
          <p className="text-sm font-medium text-foreground">尚未导入聊天记录备份</p>
          <p className="mt-1 text-xs text-muted-foreground/60">可导入 nt_msg.db 或 nt_msg_db_util 生成的 nt_msg_export.db。</p>
        </div>
      ) : null}
    </section>
  )
}
