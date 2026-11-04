import React, { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useTranslation } from 'react-i18next';
import { Users, Plus, Check, Trash, PencilSimple } from '@phosphor-icons/react';
import { DsButton } from '@/components/ui/DsButton';
import { Input } from '@/components/ui/shad/Input';
import { Badge } from '@/components/ui/shad/Badge';
import { DsAlertDialog } from '@/components/ui/DsDialog';
import { showGlobalNotification } from '@/components/UnifiedNotification';
import { getErrorMessage } from '@/utils/errorUtils';
import { SettingsGroup } from './settingsTabPrimitives';

interface ProfileInfo {
  id: string;
  name: string;
  created_at_ms: number;
  last_used_at_ms: number;
  active: boolean;
  is_default: boolean;
  data_root: string;
}

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

/**
 * 账套管理（多用户）。
 *
 * 账套 = 独立数据根：学习资料、对话、学习进度、模型 Key、飞书/微信通道
 * 绑定全部隔离。切换账套会重启应用（与数据治理恢复切换同一模式）。
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

  const locale = i18n.language || 'zh-CN';

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
            {profiles.map((p) => (
              <div
                key={p.id}
                className="flex flex-col gap-2 px-1 py-3 md:flex-row md:items-center md:justify-between"
              >
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
                      <span className="inline-flex items-center text-sm font-medium text-foreground">
                        <Users className="mr-1.5 h-4 w-4 text-muted-foreground" weight="duotone" />
                        {p.name}
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
                    disabled={busy || restarting || p.active}
                    onClick={() => {
                      setRenamingId(p.id);
                      setRenameValue(p.name);
                    }}
                  >
                    <PencilSimple className="h-4 w-4" />
                    {t('settings:profiles.rename')}
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
            ))}
          </div>
        )}
      </SettingsGroup>

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
