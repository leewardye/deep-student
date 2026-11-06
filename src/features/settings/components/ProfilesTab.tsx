import React, { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useTranslation } from 'react-i18next';
import { Check, PencilSimple, Plus, Trash, Users } from '@phosphor-icons/react';
import { DsButton } from '@/components/ui/DsButton';
import { Input } from '@/components/ui/shad/Input';
import { Label } from '@/components/ui/shad/Label';
import { Badge } from '@/components/ui/shad/Badge';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/shad/Select';
import { DsAlertDialog } from '@/components/ui/DsDialog';
import { showGlobalNotification } from '@/components/UnifiedNotification';
import { getErrorMessage } from '@/utils/errorUtils';
import { SettingsGroup } from './settingsTabPrimitives';
import { LearnerProfileSection } from './LearnerProfileSection';

/** 与 profiles.rs 的 ProfileInfo 对齐（含 0.14.0 学生身份层） */
interface ProfileInfo {
  id: string;
  name: string;
  created_at_ms: number;
  last_used_at_ms: number;
  active: boolean;
  is_default: boolean;
  data_root: string;
  student_name: string | null;
  grade: string | null;
  school: string | null;
  avatar: string | null;
}

interface IdentityDraft {
  avatar: string;
  student_name: string;
  grade: string; // '' = 未设置
  school: string;
}

/** 头像 emoji 候选（零资源方案；其余 emoji 可粘贴输入） */
const AVATAR_EMOJIS = ['🧒', '👦', '👧', '🧑', '👩', '👨', '🐣', '🦊', '🐼', '🐯', '🦉', '🐙'];

const GRADE_KEYS = [
  'primary_1', 'primary_2', 'primary_3', 'primary_4', 'primary_5', 'primary_6',
  'junior_1', 'junior_2', 'junior_3',
  'senior_1', 'senior_2', 'senior_3',
  'college', 'other',
] as const;

type ConfirmState =
  | { kind: 'none' }
  | { kind: 'switch'; profile: ProfileInfo }
  | { kind: 'delete'; profile: ProfileInfo }
  | { kind: 'rename'; profile: ProfileInfo; name: string };

function formatTime(ms: number, locale: string): string {
  if (!ms) return '-';
  try {
    return new Date(ms).toLocaleString(locale, { hour12: false });
  } catch {
    return new Date(ms).toISOString();
  }
}

/** 按 profile id 稳定派生头像色相（与 StudentSwitcher 同源规则） */
const AVATAR_HUES = [8, 32, 88, 145, 175, 210, 262, 320];
function avatarHue(id: string): number {
  let hash = 0;
  for (let i = 0; i < id.length; i += 1) {
    hash = (hash * 31 + id.charCodeAt(i)) >>> 0;
  }
  return AVATAR_HUES[hash % AVATAR_HUES.length];
}
const AVATAR_BG_CLASS = 'bg-[hsl(var(--avatar-h)_70%_50%_/_0.18)]';
const AVATAR_TEXT_CLASS = 'text-[hsl(var(--avatar-h)_65%_45%)]';

/**
 * 学生档案（多用户账套）。
 *
 * 账套 = 独立数据根：学习资料、对话、学习进度、模型 Key、飞书/微信通道
 * 绑定全部隔离。切换账套会重启应用（与数据治理恢复切换同一模式）。
 * 0.14.0：账套升级为「学生档案」——身份信息（姓名/年级/学校/emoji 头像）
 * 编辑走 profile_update；下方为学习者画像（随对话注入 AI 的策展长期层）。
 */
export function ProfilesTab() {
  const { t, i18n } = useTranslation(['settings', 'common']);
  const [profiles, setProfiles] = useState<ProfileInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [restarting, setRestarting] = useState(false);
  const [newName, setNewName] = useState('');
  const [creating, setCreating] = useState(false);
  const [renamingId, setRenamingId] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState('');
  const [editingId, setEditingId] = useState<string | null>(null);
  const [identityDraft, setIdentityDraft] = useState<IdentityDraft>({
    avatar: '', student_name: '', grade: '', school: '',
  });
  const [confirm, setConfirm] = useState<ConfirmState>({ kind: 'none' });

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const list = await invoke<ProfileInfo[]>('profile_list');
      setProfiles(list);
    } catch (e) {
      showGlobalNotification('error', getErrorMessage(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const handleCreate = async () => {
    const name = newName.trim();
    if (!name) return;
    setCreating(true);
    try {
      await invoke('profile_create', { name });
      setNewName('');
      showGlobalNotification('success', t('settings:profiles.create_success'));
      await refresh();
    } catch (e) {
      showGlobalNotification('error', getErrorMessage(e));
    } finally {
      setCreating(false);
    }
  };

  const handleSwitch = async (profile: ProfileInfo) => {
    setBusy(true);
    try {
      await invoke('profile_switch', { profileId: profile.id });
      // 后端写入标记后会重启进程；正常情况下下面不会执行到
      setRestarting(true);
    } catch (e) {
      setBusy(false);
      // 重启过程中 WebView 可能已断开，invoke 报错不一定代表失败
      showGlobalNotification('error', getErrorMessage(e));
    }
  };

  const handleDelete = async (profile: ProfileInfo) => {
    setBusy(true);
    try {
      await invoke('profile_delete', { profileId: profile.id });
      showGlobalNotification('success', t('settings:profiles.delete_success'));
      await refresh();
    } catch (e) {
      showGlobalNotification('error', getErrorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const handleRename = async (profile: ProfileInfo, name: string) => {
    setBusy(true);
    try {
      await invoke('profile_rename', { profileId: profile.id, name });
      showGlobalNotification('success', t('settings:profiles.rename_success'));
      await refresh();
    } catch (e) {
      showGlobalNotification('error', getErrorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const openIdentityEditor = (profile: ProfileInfo) => {
    setEditingId(profile.id);
    setIdentityDraft({
      avatar: profile.avatar ?? '',
      student_name: profile.student_name ?? '',
      grade: profile.grade ?? '',
      school: profile.school ?? '',
    });
  };

  const handleIdentitySave = async (profile: ProfileInfo) => {
    setBusy(true);
    try {
      await invoke('profile_update', {
        profileId: profile.id,
        studentName: identityDraft.student_name.trim(),
        grade: identityDraft.grade || null,
        school: identityDraft.school.trim(),
        avatar: identityDraft.avatar.trim(),
      });
      showGlobalNotification('success', t('settings:profiles.identity_save_success'));
      setEditingId(null);
      await refresh();
    } catch (e) {
      showGlobalNotification('error', getErrorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const locale = i18n.language || 'zh-CN';

  const renderAvatar = (p: ProfileInfo, sizeClass = 'size-6') => {
    const hue = avatarHue(p.id);
    const fallback = (p.student_name || p.name).charAt(0) || '·';
    return (
      <span
        aria-hidden
        className={`inline-flex shrink-0 items-center justify-center rounded-full text-xs leading-none ${sizeClass} ${AVATAR_BG_CLASS} ${AVATAR_TEXT_CLASS}`}
        style={{ ['--avatar-h' as string]: hue }}
      >
        {p.avatar || fallback}
      </span>
    );
  };

  const gradeText = (grade: string | null): string | null =>
    grade ? t(`settings:grades.${grade}`, { defaultValue: grade }) : null;

  return (
    <div className="flex flex-col gap-6 pb-10">
      <SettingsGroup
        title={t('settings:profiles.title')}
        description={t('settings:profiles.description')}
        actions={
          <div className="flex items-center gap-2">
            <Input
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && newName.trim() && !creating) void handleCreate();
              }}
              placeholder={t('settings:profiles.name_placeholder')}
              className="h-9 w-44"
              maxLength={40}
              disabled={busy || restarting}
            />
            <DsButton
              variant="default"
              size="sm"
              disabled={!newName.trim() || creating || busy || restarting}
              onClick={() => void handleCreate()}
            >
              <Plus className="mr-1 h-4 w-4" weight="bold" />
              {t('settings:profiles.create')}
            </DsButton>
          </div>
        }
      >
        {loading ? (
          <div className="px-1 py-6 text-sm text-muted-foreground">{t('settings:loading')}</div>
        ) : (
          <div className="flex flex-col divide-y divide-border/60">
            {profiles.map((p) => {
              const pGrade = gradeText(p.grade);
              const identityEmpty = !p.student_name && !p.grade && !p.school;
              return (
                <div key={p.id} className="flex flex-col gap-2 px-1 py-3">
                  <div className="flex flex-col gap-2 md:flex-row md:items-center md:justify-between">
                    <div className="min-w-0 flex-1">
                      {renamingId === p.id ? (
                        <div className="flex items-center gap-2">
                          <Input
                            value={renameValue}
                            onChange={(e) => setRenameValue(e.target.value)}
                            onKeyDown={(e) => {
                              if (e.key === 'Enter' && renameValue.trim()) {
                                void handleRename(p, renameValue.trim());
                              } else if (e.key === 'Escape') {
                                setRenamingId(null);
                              }
                            }}
                            className="h-8 w-48"
                            maxLength={40}
                            autoFocus
                            disabled={busy}
                          />
                          <DsButton
                            variant="default"
                            size="sm"
                            disabled={busy || !renameValue.trim()}
                            onClick={() => void handleRename(p, renameValue.trim())}
                          >
                            {t('common:save')}
                          </DsButton>
                          <DsButton
                            variant="ghost"
                            size="sm"
                            disabled={busy}
                            onClick={() => setRenamingId(null)}
                          >
                            {t('common:cancel')}
                          </DsButton>
                        </div>
                      ) : (
                        <div className="flex flex-wrap items-center gap-2">
                          {renderAvatar(p)}
                          <span className="inline-flex items-center text-sm font-medium text-foreground">
                            {p.student_name || p.name}
                          </span>
                          {p.active && (
                            <Badge variant="default" className="gap-1">
                              <Check className="h-3 w-3" weight="bold" />
                              {t('settings:profiles.active')}
                            </Badge>
                          )}
                          {p.is_default && (
                            <Badge variant="secondary">{t('settings:profiles.default')}</Badge>
                          )}
                          {pGrade && (
                            <Badge variant="outline" className="text-xs font-normal">
                              {pGrade}
                            </Badge>
                          )}
                          {p.school && (
                            <span className="truncate text-xs text-muted-foreground">
                              {p.school}
                            </span>
                          )}
                          {identityEmpty && (
                            <span className="text-xs text-muted-foreground/70">
                              {t('settings:profiles.identity_empty_hint')}
                            </span>
                          )}
                        </div>
                      )}
                      <p className="mt-1 truncate text-xs text-muted-foreground/70" title={p.data_root}>
                        {t('settings:profiles.last_used')}: {formatTime(p.last_used_at_ms, locale)}
                        {' · '}
                        {p.data_root}
                      </p>
                    </div>
                    <div className="flex flex-shrink-0 items-center gap-2">
                      <DsButton
                        variant="ghost"
                        size="sm"
                        disabled={busy || restarting || renamingId === p.id}
                        onClick={() => {
                          if (editingId === p.id) {
                            setEditingId(null);
                          } else {
                            openIdentityEditor(p);
                          }
                        }}
                      >
                        <PencilSimple className="h-4 w-4" />
                        {editingId === p.id
                          ? t('common:cancel')
                          : t('settings:profiles.edit_identity')}
                      </DsButton>
                      {!p.active && (
                        <DsButton
                          variant="default"
                          size="sm"
                          disabled={busy || restarting}
                          onClick={() => setConfirm({ kind: 'switch', profile: p })}
                        >
                          {t('settings:profiles.switch')}
                        </DsButton>
                      )}
                      <DsButton
                        variant="ghost"
                        size="sm"
                        disabled={busy || restarting || p.active || p.is_default}
                        className="text-destructive hover:text-destructive"
                        onClick={() => setConfirm({ kind: 'delete', profile: p })}
                      >
                        <Trash className="h-4 w-4" />
                        {t('settings:profiles.delete')}
                      </DsButton>
                    </div>
                  </div>

                  {editingId === p.id && (
                    <div className="flex flex-col gap-3 rounded-md border border-border/60 bg-muted/30 p-3">
                      {/* 头像 emoji */}
                      <div className="flex flex-col gap-1.5">
                        <Label className="text-xs text-muted-foreground">
                          {t('settings:profiles.avatar')}
                        </Label>
                        <div className="flex flex-wrap items-center gap-1">
                          {AVATAR_EMOJIS.map((emoji) => (
                            // eslint-disable-next-line ds-components/no-native-button -- emoji 色板格子，DsButton 形态过大
                            <button
                              key={emoji}
                              type="button"
                              aria-label={`${t('settings:profiles.avatar')} ${emoji}`}
                              aria-pressed={identityDraft.avatar === emoji}
                              className={`inline-flex size-8 items-center justify-center rounded-md text-base transition-colors ${
                                identityDraft.avatar === emoji
                                  ? 'bg-primary/15 ring-1 ring-primary/40'
                                  : 'hover:bg-accent'
                              }`}
                              onClick={() =>
                                setIdentityDraft((prev) => ({
                                  ...prev,
                                  avatar: prev.avatar === emoji ? '' : emoji,
                                }))
                              }
                            >
                              {emoji}
                            </button>
                          ))}
                          <Input
                            value={identityDraft.avatar}
                            onChange={(e) =>
                              setIdentityDraft((prev) => ({ ...prev, avatar: e.target.value }))
                            }
                            placeholder={t('settings:profiles.avatar_custom')}
                            className="h-8 w-24 text-center"
                            maxLength={8}
                            disabled={busy}
                          />
                        </div>
                      </div>
                      {/* 姓名 / 年级 / 学校 */}
                      <div className="grid gap-3 md:grid-cols-3">
                        <div className="flex flex-col gap-1.5">
                          <Label className="text-xs text-muted-foreground">
                            {t('settings:profiles.student_name')}
                          </Label>
                          <Input
                            value={identityDraft.student_name}
                            onChange={(e) =>
                              setIdentityDraft((prev) => ({
                                ...prev,
                                student_name: e.target.value,
                              }))
                            }
                            placeholder={t('settings:profiles.student_name_placeholder')}
                            maxLength={20}
                            disabled={busy}
                          />
                        </div>
                        <div className="flex flex-col gap-1.5">
                          <Label className="text-xs text-muted-foreground">
                            {t('settings:profiles.grade')}
                          </Label>
                          <Select
                            value={identityDraft.grade || undefined}
                            onValueChange={(v) =>
                              setIdentityDraft((prev) => ({ ...prev, grade: v === '__none__' ? '' : v }))
                            }
                          >
                            <SelectTrigger className="h-9 w-full" aria-label={t('settings:profiles.grade')}>
                              <SelectValue
                                placeholder={t('settings:profiles.grade_placeholder')}
                              />
                            </SelectTrigger>
                            <SelectContent>
                              <SelectItem value="__none__">
                                {t('settings:profiles.grade_placeholder')}
                              </SelectItem>
                              {GRADE_KEYS.map((g) => (
                                <SelectItem key={g} value={g}>
                                  {t(`settings:grades.${g}`)}
                                </SelectItem>
                              ))}
                            </SelectContent>
                          </Select>
                        </div>
                        <div className="flex flex-col gap-1.5">
                          <Label className="text-xs text-muted-foreground">
                            {t('settings:profiles.school')}
                          </Label>
                          <Input
                            value={identityDraft.school}
                            onChange={(e) =>
                              setIdentityDraft((prev) => ({ ...prev, school: e.target.value }))
                            }
                            placeholder={t('settings:profiles.school_placeholder')}
                            maxLength={40}
                            disabled={busy}
                          />
                        </div>
                      </div>
                      <p className="text-xs text-muted-foreground/70">
                        {t('settings:profiles.identity_usage_hint')}
                      </p>
                      <DsButton
                        variant="default"
                        size="sm"
                        className="self-start"
                        disabled={busy}
                        onClick={() => void handleIdentitySave(p)}
                      >
                        {t('common:save')}
                      </DsButton>
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </SettingsGroup>

      <LearnerProfileSection />

      {restarting && (
        <div className="rounded-md bg-muted/60 px-4 py-3 text-sm text-foreground/80">
          {t('settings:profiles.restarting')}
        </div>
      )}

      <DsAlertDialog
        open={confirm.kind === 'switch'}
        onOpenChange={(open) => {
          if (!open) setConfirm({ kind: 'none' });
        }}
        title={t('settings:profiles.switch_confirm_title')}
        description={t('settings:profiles.switch_confirm_desc', {
          name: confirm.kind === 'switch' ? confirm.profile.name : '',
        })}
        confirmText={t('settings:profiles.switch')}
        cancelText={t('common:cancel')}
        onConfirm={async () => {
          if (confirm.kind === 'switch') {
            await handleSwitch(confirm.profile);
          }
        }}
      />

      <DsAlertDialog
        open={confirm.kind === 'delete'}
        onOpenChange={(open) => {
          if (!open) setConfirm({ kind: 'none' });
        }}
        title={t('settings:profiles.delete_confirm_title')}
        description={t('settings:profiles.delete_confirm_desc', {
          name: confirm.kind === 'delete' ? confirm.profile.name : '',
        })}
        confirmText={t('common:delete')}
        cancelText={t('common:cancel')}
        onConfirm={async () => {
          if (confirm.kind === 'delete') {
            await handleDelete(confirm.profile);
          }
        }}
      />
    </div>
  );
}

export default ProfilesTab;
