/**
 * 会话输入框收件箱：别的页面（如 PDF 框选截图）把文件交给某个会话的输入框，
 * 由输入框走和粘贴 / 拖入同一条附件上传流程（HEIC 转码、VFS 上传、ContextRef、处理进度）。
 *
 * 目标输入框可能还没挂载（经典壳从没进过聊天页、学习桌面里对话窗口没开）：
 * 先按会话存着，输入框就绪后取走。只在内存里，刷新即丢。
 */
import { useEffect, useRef } from 'react';
import { useEventRegistry } from '@/hooks/useEventRegistry';

export const CHAT_INPUT_INBOX_EVENT = 'chat-v2:input-inbox';

/** 一直没人取走的文件最多留这么久 */
const PENDING_TTL_MS = 30 * 60_000;
/** 防止反复投递却从不打开聊天时无限堆积 */
const MAX_PENDING_ENTRIES = 20;

interface PendingEntry {
  sessionId: string;
  files: File[];
  /** 预填文本（AI 辅助等场景）；与 files 同条目送达 */
  text?: string;
  at: number;
}

let pending: PendingEntry[] = [];

function prune(now: number): void {
  pending = pending.filter((entry) => now - entry.at <= PENDING_TTL_MS).slice(-MAX_PENDING_ENTRIES);
}

/** 把文件交给指定会话的输入框（已挂载的立即处理，否则等它就绪） */
export function queueChatInputFiles(sessionId: string, files: File[]): void {
  if (!sessionId || files.length === 0) return;
  pending.push({ sessionId, files, at: Date.now() });
  prune(Date.now());
  window.dispatchEvent(new CustomEvent(CHAT_INPUT_INBOX_EVENT, { detail: { sessionId } }));
}

/**
 * 把预填文本交给指定会话的输入框（AI 辅助完善画像等跨页场景）。
 * 已挂载的输入框立即填入；未挂载的等就绪后取走。同一条目内 text 与 files 互不干扰。
 */
export function queueChatInputText(sessionId: string, text: string): void {
  const trimmed = text.trim();
  if (!sessionId || !trimmed) return;
  pending.push({ sessionId, files: [], text: trimmed, at: Date.now() });
  prune(Date.now());
  window.dispatchEvent(new CustomEvent(CHAT_INPUT_INBOX_EVENT, { detail: { sessionId } }));
}

/** 取走某个会话的待处理文件（按投递顺序）；取走即出队，同一会话多个输入框只会有一个拿到 */
export function takeChatInputFiles(sessionId: string): File[] {
  prune(Date.now());
  const mine = pending.filter((entry) => entry.sessionId === sessionId && entry.files.length > 0);
  if (mine.length === 0) return [];
  pending = pending.filter((entry) => !(entry.sessionId === sessionId && entry.files.length > 0));
  return mine.flatMap((entry) => entry.files);
}

/** 取走某个会话的预填文本（与文件同一 FIFO；文本条目只有 text，无文件） */
export function takeChatInputText(sessionId: string): string | null {
  prune(Date.now());
  const hasText = (entry: PendingEntry) => entry.sessionId === sessionId && typeof entry.text === 'string';
  const mine = pending.filter(hasText);
  if (mine.length === 0) return null;
  pending = pending.filter((entry) => !hasText(entry));
  return mine.map((entry) => entry.text ?? '').join('\n\n');
}

/** 测试用：清空收件箱 */
export function resetChatInputInbox(): void {
  pending = [];
}

/**
 * 输入框侧：就绪后取走本会话的待处理文件，之后每次有新投递再取。
 * 未就绪时不取（附件管线要等输入框完成延迟初始化）。
 * `onText` 可选：提供时同样取走预填文本（不依赖附件管线就绪语义之外的额外初始化）。
 */
export function useChatInputInbox(
  sessionId: string | undefined,
  ready: boolean,
  onFiles: (files: File[]) => void,
  onText?: (text: string) => void,
): void {
  const onFilesRef = useRef(onFiles);
  onFilesRef.current = onFiles;
  const onTextRef = useRef(onText);
  onTextRef.current = onText;

  useEffect(() => {
    if (!sessionId || !ready) return;
    const files = takeChatInputFiles(sessionId);
    if (files.length > 0) onFilesRef.current(files);
    const text = takeChatInputText(sessionId);
    if (text && onTextRef.current) onTextRef.current(text);
  }, [sessionId, ready]);

  useEventRegistry(
    sessionId && ready
      ? [{
          target: 'window',
          type: CHAT_INPUT_INBOX_EVENT,
          listener: (event) => {
            if ((event as CustomEvent<{ sessionId?: string }>).detail?.sessionId !== sessionId) return;
            const files = takeChatInputFiles(sessionId);
            if (files.length > 0) onFilesRef.current(files);
            const text = takeChatInputText(sessionId);
            if (text && onTextRef.current) onTextRef.current(text);
          },
        }]
      : [],
    [sessionId, ready],
  );
}
