import React, { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useTranslation } from 'react-i18next';
import { CaretDown, Check, Plus, GearSix } from '@phosphor-icons/react';
import {
  AppMenu,
  AppMenuTrigger,
  AppMenuContent,
  AppMenuGroup,
  AppMenuItem,
} from '@/components/ui/app-menu/AppMenu';
import { DsAlertDialog } from '@/components/ui/DsDialog';
import { showGlobalNotification } from '@/components/UnifiedNotification';
import { getErrorMessage } from '@/utils/errorUtils';
import { setPendingSettingsTab } from '@/utils/pendingSettingsTab';
import { APP_EVENTS, dispatchAppEvent } from '@/events';

/** 与 profiles.rs 的 ProfileInfo 对齐（本组件只用其中一部分） */
export interface StudentProfileInfo {
  id: string;
  name: string;
  active: boolean;
  student_name: string | null;
  grade: string | null;
  avatar: string | null;
}

/** 按 profile id 稳定派生头像色相（8 档），经 --avatar-h 注入主题类，不硬编码颜色 */
const AVATAR_HUES = [8, 32, 88, 145, 175, 210, 262, 320];
function avatarHue(id: string): number {
  let hash = 0;
  for (let i = 0; i < id.length; i += 1) {
    hash = (hash * 31 + id.charCodeAt(i)) >>> 0;
  }
  return AVATAR_HUES[hash % AVATAR_HUES.length];
}

/** 头像圆片通用类：底色/文字色均由 --avatar-h 驱动（随明暗主题可读） */
const AVATAR_BG_CLASS = 'bg-[hsl(var(--avatar-h)_70%_50%_/_0.18)]';
const AVATAR_TEXT_CLASS = 'text-[hsl(var(--avatar-h)_65%_45%)]';

function gradeLabel(grade: string | null, t: (k: string, opts?: Record<string, unknown>) => string): string | null {
  if (!grade) return null;
  return t(`settings:grades.${grade}`, { defaultValue: grade });
}

/**
 * 桌面顶栏「当前学生」标识与切换。
 *
 * 学生 = 账套（profiles）。切换会重启应用（数据根隔离，与设置 → 学生档案
 * 中的切换同一语义），点击菜单项先弹确认。身份编辑入口在设置页。
 */
export function StudentSwitcher({ className }: { className?: string }) {
  const { t } = useTranslation(['settings', 'common']);
  const [profiles, setProfiles] = useState<StudentProfileInfo[]>([]);
  const [open, setOpen] = useState(false);
  const [switchTarget, setSwitchTarget] = useState<StudentProfileInfo | null>(null);
  const [restarting, setRestarting] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const list = await invoke<StudentProfileInfo[]>('profile_list');
      setProfiles(list);
    } catch {
      // 顶栏标识是装饰性信息，失败静默（不弹通知打扰）
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 菜单每次打开时刷新（新建/改名/编辑身份后回来能看到最新）
  const handleOpenChange = useCallback(
    (next: boolean) => {
      setOpen(next);
      if (next) void refresh();
    },
    [refresh],
  );

  const active = profiles.find((p) => p.active) ?? null;
  const displayName = active ? (active.student_name || active.name) : '';
  const avatarText = active ? (active.avatar || displayName.charAt(0) || '·') : '·';
  const hue = active ? avatarHue(active.id) : 210;
  const gradeText = active ? gradeLabel(active.grade, t) : null;

  const goManage = useCallback(() => {
    setOpen(false);
    setPendingSettingsTab('profiles');
    dispatchAppEvent(APP_EVENTS.NAVIGATE_TO_TAB, { tabName: 'settings' });
  }, []);

  const handleSwitch = useCallback(async (target: StudentProfileInfo) => {
    setRestarting(true);
    try {
      await invoke('profile_switch', { profileId: target.id });
      // 后端写完标记会重启进程；正常不会走到这里
    } catch (e) {
      setRestarting(false);
      // 重启过程中 WebView 可能已断开，invoke 报错不一定代表失败
      showGlobalNotification('error', getErrorMessage(e));
    }
  }, []);

  return (
    <div className={className} data-no-drag data-tour-id="student-switcher">
      <AppMenu open={open} onOpenChange={handleOpenChange} mode="dropdown">
        <AppMenuTrigger
          aria-label={t('settings:profiles.current_student')}
          className="inline-flex h-8 max-w-[220px] items-center gap-1.5 rounded-full border border-border/70 bg-background/60 px-2 text-foreground shadow-sm transition-colors hover:bg-accent/50"
        >
          <span
            aria-hidden
            className={`inline-flex size-5 shrink-0 items-center justify-center rounded-full text-2xs leading-none ${AVATAR_BG_CLASS} ${AVATAR_TEXT_CLASS}`}
            style={{ '--avatar-h': hue } as React.CSSProperties}
          >
            {avatarText}
          </span>
          <span className="min-w-0 truncate text-xs font-medium">
            {displayName || t('settings:profiles.unset_student')}
          </span>
          {gradeText ? (
            <span className="hidden shrink-0 rounded bg-muted px-1 py-px text-2xs text-muted-foreground lg:inline">
              {gradeText}
            </span>
          ) : null}
          <CaretDown size={10} className="shrink-0 text-muted-foreground" weight="bold" />
        </AppMenuTrigger>
        <AppMenuContent align="end" width={240}>
          <AppMenuGroup label={t('settings:profiles.current_student')}>
            {profiles.map((p) => {
              const name = p.student_name || p.name;
              const pGrade = gradeLabel(p.grade, t);
              const pAvatar = p.avatar || name.charAt(0) || '·';
              const pHue = avatarHue(p.id);
              return (
                <AppMenuItem
                  key={p.id}
                  disabled={p.active || restarting}
                  onClick={() => {
                    setOpen(false);
                    setSwitchTarget(p);
                  }}
                >
                  <span className="flex min-w-0 flex-1 items-center gap-2">
                    <span
                      aria-hidden
                      className={`inline-flex size-5 shrink-0 items-center justify-center rounded-full text-2xs leading-none ${AVATAR_BG_CLASS} ${AVATAR_TEXT_CLASS}`}
                      style={{ '--avatar-h': pHue } as React.CSSProperties}
                    >
                      {pAvatar}
                    </span>
                    <span className="min-w-0 truncate">{name}</span>
                    {pGrade ? (
                      <span className="shrink-0 text-xs text-muted-foreground">{pGrade}</span>
                    ) : null}
                  </span>
                  {p.active ? <Check size={14} weight="bold" className="shrink-0 text-primary" /> : null}
                </AppMenuItem>
              );
            })}
          </AppMenuGroup>
          <AppMenuGroup>
            <AppMenuItem
              icon={<Plus size={16} />}
              disabled={restarting}
              onClick={goManage}
            >
              {t('settings:profiles.students_add')}
            </AppMenuItem>
            <AppMenuItem
              icon={<GearSix size={16} />}
              disabled={restarting}
              onClick={goManage}
            >
              {t('settings:profiles.students_manage')}
            </AppMenuItem>
          </AppMenuGroup>
        </AppMenuContent>
      </AppMenu>

      <DsAlertDialog
        open={switchTarget !== null}
        onOpenChange={(next) => {
          if (!next) setSwitchTarget(null);
        }}
        title={t('settings:profiles.switch_confirm_title')}
        description={t('settings:profiles.switch_confirm_desc', {
          name: switchTarget ? (switchTarget.student_name || switchTarget.name) : '',
        })}
        confirmText={t('settings:profiles.switch')}
        cancelText={t('common:cancel')}
        onConfirm={async () => {
          if (switchTarget) await handleSwitch(switchTarget);
        }}
      />

      {restarting ? (
        <div
          className="fixed inset-0 z-modal flex items-center justify-center bg-background/80"
          role="alert"
        >
          <div className="rounded-lg border border-border bg-card px-6 py-4 text-sm text-foreground shadow-lg">
            {t('settings:profiles.restarting')}
          </div>
        </div>
      ) : null}
    </div>
  );
}

export default StudentSwitcher;
