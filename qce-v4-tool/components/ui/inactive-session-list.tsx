"use client"

import { useEffect, useMemo, useRef, useState } from "react"
import {
  AlertCircle,
  ChevronLeft,
  ChevronRight,
  Filter,
  Info,
  RefreshCw,
  Search,
  UserMinus,
  Users,
  X,
} from "lucide-react"
import type { InactiveSession, InactiveSessionsResponse } from "@/types/api"
import { formatRelativeFromNow } from "@/lib/session-sort"
import { Avatar, AvatarFallback, AvatarImage } from "./avatar"
import { Button } from "./button"
import { Input } from "./input"
import { Loader } from "./loader"
import { PillDropdown } from "./pill-dropdown"

type InactiveFilter = 'all' | InactiveSession['kind']
type InactiveSort = 'last_message' | 'message_count'

const PAGE_SIZE = 50

interface InactiveSessionListProps {
  data: InactiveSessionsResponse | null
  loading: boolean
  error: string | null
  onRefresh: () => void
  onPreview: (session: InactiveSession) => void
  onExport: (session: InactiveSession) => void
  onManualLookup: () => void
}

function sessionDisplayName(session: InactiveSession): string {
  if (session.kind === 'unavailable_group' || session.kind === 'discussion') {
    const groupCode = session.peerUin || session.peerUid
    const name = session.name.trim()
    const fallbackPrefix = session.kind === 'discussion' ? '讨论组' : '群聊'
    const isFallbackName = !name
      || name === groupCode
      || name === `${fallbackPrefix} ${groupCode}`
    return isFallbackName ? `${fallbackPrefix} ${groupCode}` : `${name}（${groupCode}）`
  }

  const qq = session.peerUin || (/^\d+$/.test(session.peerUid) ? session.peerUid : '')
  const name = session.name.trim()
  const isFallbackName = !name
    || name === session.peerUid
    || name === qq
    || name === `QQ ${qq}`

  if (!qq) return name || session.peerUid
  return isFallbackName ? `QQ ${qq}` : `${name}（${qq}）`
}

function sessionKindLabel(kind: InactiveSession['kind']): string {
  if (kind === 'non_friend') return '已删除 / 非好友'
  if (kind === 'discussion') return '讨论组'
  return '已退出 / 不可用群'
}

export function InactiveSessionList({
  data,
  loading,
  error,
  onRefresh,
  onPreview,
  onExport,
  onManualLookup,
}: InactiveSessionListProps) {
  const [search, setSearch] = useState("")
  const [filter, setFilter] = useState<InactiveFilter>('all')
  const [sort, setSort] = useState<InactiveSort>('last_message')
  const [page, setPage] = useState(1)
  const searchInputRef = useRef<HTMLInputElement>(null)

  const filteredSessions = useMemo(() => {
    const normalizedSearch = search.trim().toLowerCase()
    return [...(data?.sessions ?? [])]
      .filter((session) => filter === 'all' || session.kind === filter)
      .filter((session) => {
        if (!normalizedSearch) return true
        return [session.name, session.peerUid, session.peerUin]
          .filter(Boolean)
          .some((value) => String(value).toLowerCase().includes(normalizedSearch))
      })
      .sort((left, right) => {
        const leftTime = left.lastMsgTime ? Date.parse(left.lastMsgTime) : 0
        const rightTime = right.lastMsgTime ? Date.parse(right.lastMsgTime) : 0
        const leftCount = left.messageCount ?? -1
        const rightCount = right.messageCount ?? -1
        if (sort === 'message_count' && leftCount !== rightCount) return rightCount - leftCount
        if (leftTime !== rightTime) return rightTime - leftTime
        if (sort === 'last_message' && leftCount !== rightCount) return rightCount - leftCount
        return left.name.localeCompare(right.name, 'zh-CN')
      })
  }, [data?.sessions, filter, search, sort])

  const totalPages = Math.max(1, Math.ceil(filteredSessions.length / PAGE_SIZE))
  const pageItems = useMemo(() => {
    const start = (page - 1) * PAGE_SIZE
    return filteredSessions.slice(start, start + PAGE_SIZE)
  }, [filteredSessions, page])

  useEffect(() => {
    setPage(1)
  }, [filter, search, sort])

  useEffect(() => {
    if (page > totalPages) setPage(totalPages)
  }, [page, totalPages])

  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      const typing = document.activeElement?.tagName === 'INPUT'
      if (event.key === '/' && !typing) {
        event.preventDefault()
        searchInputRef.current?.focus()
      }
      if (event.key === 'Escape' && search) {
        setSearch("")
        searchInputRef.current?.blur()
      }
    }
    window.addEventListener('keydown', handleKeyDown)
    return () => window.removeEventListener('keydown', handleKeyDown)
  }, [search])

  return (
    <div className="space-y-4">
      <div className="rounded-xl bg-black/[0.025] px-4 py-3 text-[12px] leading-relaxed text-muted-foreground dark:bg-white/[0.035]">
        <div className="flex items-start gap-2">
          <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <p>
            此处比较当前好友、群列表与 QQ 会话索引；导入 nt_msg.db 后还会合并消息表中出现过的全部私聊、群聊和旧版讨论组。非好友可能包含已删除、已注销或陌生人；不可用群可能包含退群、被移出或群解散。
          </p>
        </div>
        {data && data.source !== 'database' && (
          <div className="mt-2 flex items-start gap-2 text-amber-700 dark:text-amber-400">
            <AlertCircle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <p>当前列表仅来自 QQ/NapCat 会话索引，它可能只保留最近或仍活跃的少量会话；导入下方的 nt_msg.db 才能枚举消息表中的历史对象。</p>
          </div>
        )}
        {data?.source === 'database' && (
          <div className="mt-2 flex items-start gap-2 text-emerald-700 dark:text-emerald-400">
            <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <p>已合并导入数据库中的 {data.databaseRawCount?.toLocaleString() ?? 0} 个历史会话，并排除仍在当前好友或群列表中的对象。</p>
          </div>
        )}
      </div>

      <div className="flex flex-col items-center gap-1.5 sm:flex-row">
        <div className="relative w-full flex-1">
          <Search className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground/60" />
          <Input
            ref={searchInputRef}
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="搜索名称、QQ号或群号..."
            className="h-8 rounded-full border border-black/[0.03] bg-white pl-9 pr-8 text-[13px] shadow-[0_1px_2px_rgba(0,0,0,0.02)] focus-visible:ring-0 dark:border-white/10 dark:bg-neutral-900"
          />
          {search && (
            <button
              type="button"
              aria-label="清除搜索"
              onClick={() => setSearch("")}
              className="absolute right-2 top-1/2 -translate-y-1/2 rounded-full p-0.5 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
            >
              <X className="h-3.5 w-3.5" />
            </button>
          )}
        </div>
        <div className="flex items-center gap-2 self-end sm:self-auto">
          <PillDropdown
            value={filter}
            onChange={(value) => setFilter(value as InactiveFilter)}
            options={[
              { value: 'all', label: `全部 (${data?.totalCount ?? 0})` },
              { value: 'non_friend', label: `已删除 / 非好友 (${data?.nonFriendCount ?? 0})` },
              { value: 'unavailable_group', label: `已退出 / 不可用群 (${data?.unavailableGroupCount ?? 0})` },
              { value: 'discussion', label: `讨论组 (${data?.discussionCount ?? 0})` },
            ]}
          />
          <PillDropdown
            value={sort}
            onChange={(value) => setSort(value as InactiveSort)}
            options={[
              { value: 'last_message', label: '按最后消息时间' },
              { value: 'message_count', label: '按聊天记录条数' },
            ]}
          />
          <Button
            type="button"
            variant="ghost"
            size="icon"
            aria-label="刷新已删除/退出会话"
            onClick={onRefresh}
            disabled={loading}
            className="h-8 w-8 rounded-full"
          >
            <RefreshCw className={`h-3.5 w-3.5 ${loading ? 'animate-spin' : ''}`} />
          </Button>
        </div>
      </div>

      {loading && !data ? (
        <div className="flex flex-col items-center justify-center py-20 text-sm text-muted-foreground">
          <Loader size={24} className="mb-3" />
          正在读取本机历史会话…
        </div>
      ) : error ? (
        <div className="flex flex-col items-center justify-center py-20 text-center">
          <AlertCircle className="mb-3 h-8 w-8 text-red-500/70" />
          <p className="text-sm font-medium text-foreground">无法加载已删除/退出会话</p>
          <p className="mt-1 max-w-lg text-xs text-muted-foreground">{error}</p>
          <Button type="button" variant="outline" size="sm" onClick={onRefresh} className="mt-4 rounded-full">
            重试
          </Button>
        </div>
      ) : filteredSessions.length === 0 ? (
        <div className="flex flex-col items-center justify-center py-20 text-center">
          <Filter className="mb-3 h-9 w-9 text-muted-foreground/20" />
          <p className="text-sm font-medium text-foreground">
            {data?.totalCount ? '没有符合条件的会话' : '本机索引中没有发现已删除或退出的会话'}
          </p>
          <p className="mt-1 text-xs text-muted-foreground/60">列表可能不完整，仍可按 QQ 号或群号手动查询。</p>
          <Button type="button" variant="ghost" size="sm" onClick={onManualLookup} className="mt-3 rounded-full">
            按号码查询
          </Button>
        </div>
      ) : (
        <div className="flex flex-col">
          {pageItems.map((session) => {
            const isGroup = session.kind !== 'non_friend'
            const displayName = sessionDisplayName(session)
            return (
              <div
                key={`${session.chatType}_${session.peerUid}`}
                className="group flex items-center gap-2 rounded-xl px-1 transition-colors hover:bg-black/[0.03] dark:hover:bg-white/[0.03]"
              >
                <button
                  type="button"
                  onClick={() => onPreview(session)}
                  className="flex min-w-0 flex-1 items-center gap-3 px-2 py-3 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring/40"
                  aria-label={`预览 ${displayName} 聊天记录`}
                >
                  <Avatar className="h-10 w-10 shrink-0 overflow-hidden rounded-full">
                    <AvatarImage src={session.avatarUrl} alt={session.name} />
                    <AvatarFallback className="rounded-full text-xs">
                      {isGroup ? <Users className="h-4 w-4" /> : <UserMinus className="h-4 w-4" />}
                    </AvatarFallback>
                  </Avatar>
                  <span className="min-w-0 flex-1">
                    <span className="block truncate text-sm font-medium text-foreground">{displayName}</span>
                    <span className="mt-0.5 flex flex-wrap items-center gap-2 text-xs text-muted-foreground/55">
                      <span>{sessionKindLabel(session.kind)}</span>
                      {session.lastMsgTime && (
                        <>
                          <span aria-hidden className="h-2.5 w-px bg-current opacity-20" />
                          <span title={session.lastMsgTime}>{formatRelativeFromNow(session.lastMsgTime)}</span>
                        </>
                      )}
                      {typeof session.messageCount === 'number' && (
                        <>
                          <span aria-hidden className="h-2.5 w-px bg-current opacity-20" />
                          <span>{session.messageCount.toLocaleString()} 条</span>
                        </>
                      )}
                    </span>
                  </span>
                </button>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() => onExport(session)}
                  aria-label={`导出 ${session.name} 聊天记录`}
                  className="mr-2 h-7 shrink-0 rounded-full px-3 text-[12px] opacity-100 sm:opacity-0 sm:group-hover:opacity-100 sm:focus-visible:opacity-100"
                >
                  导出
                </Button>
              </div>
            )
          })}
        </div>
      )}

      {!error && !loading && data && (
        <div className="flex flex-col items-center justify-between gap-3 border-t border-black/[0.04] pt-3 text-xs text-muted-foreground/60 sm:flex-row dark:border-white/[0.06]">
          <span>
            {filteredSessions.length > 0
              ? `${(page - 1) * PAGE_SIZE + 1}-${Math.min(page * PAGE_SIZE, filteredSessions.length)} / ${filteredSessions.length}`
              : '0 条'}
          </span>
          {totalPages > 1 && (
            <div className="flex items-center gap-1">
              <Button
                type="button"
                variant="ghost"
                size="icon"
                disabled={page <= 1}
                onClick={() => setPage((current) => Math.max(1, current - 1))}
                className="h-8 w-8 rounded-full"
                aria-label="上一页"
              >
                <ChevronLeft className="h-3.5 w-3.5" />
              </Button>
              <span className="px-2 tabular-nums">{page} / {totalPages}</span>
              <Button
                type="button"
                variant="ghost"
                size="icon"
                disabled={page >= totalPages}
                onClick={() => setPage((current) => Math.min(totalPages, current + 1))}
                className="h-8 w-8 rounded-full"
                aria-label="下一页"
              >
                <ChevronRight className="h-3.5 w-3.5" />
              </Button>
            </div>
          )}
          <button type="button" onClick={onManualLookup} className="transition-colors hover:text-foreground hover:underline">
            列表中没有？按号码查询
          </button>
        </div>
      )}
    </div>
  )
}
