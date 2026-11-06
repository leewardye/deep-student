/**
 * 学习者画像（策展长期层）设置区。
 *
 * 数据源与对话内 AI 共用同一份画像（dstu-memory learner_profile）：
 * - 表单保存走 learner_profile_save（全量权威写回，可清空字段）
 * - 「AI 辅助完善」把引导 prompt 投进聊天输入框（chatInputInbox 文本通道），
 *   由对话内 AI 面试后经 learner_profile_update 工具增量写入
 * - 画像内容随每轮对话注入 system（prompt_builder），AI 全链路可感知
 */
import React, { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useTranslation } from 'react-i18next';
import { Plus, Sparkle, Trash, X } from '@phosphor-icons/react';
import { DsButton } from '@/components/ui/DsButton';
import { Input } from '@/components/ui/shad/Input';
import { Textarea } from '@/components/ui/shad/Textarea';
import { Badge } from '@/components/ui/shad/Badge';
import { Label } from '@/components/ui/shad/Label';
import { SettingsGroup } from './settingsTabPrimitives';
import { showGlobalNotification } from '@/components/UnifiedNotification';
import { getErrorMessage } from '@/utils/errorUtils';
import { APP_EVENTS, dispatchAppEvent } from '@/events';
import { ensureActiveChatSession } from '@/features/chat/pages/ensureActiveChatSession';
import { queueChatInputText } from '@/features/chat/context/chatInputInbox';

/** 与 dstu-memory learner_profile.rs 的 WeakPoint 对齐（snake_case 直传 serde） */
interface WeakPointRow {
  subject: string;
  knowledge_point: string;
  error_pattern: string;
  evidence_count: number;
  last_seen?: string | null;
  source?: string | null;
}

/** 与 LearningGoal 对齐；deadline 空串 = 无期限 */
interface GoalRow {
  goal: string;
  deadline: string;
}

interface ProfileFormState {
  explanation_style: string;
  language: string;
  pace: string;
  others: string[];
  weak_points: WeakPointRow[];
  goals: GoalRow[];
  recent_status: string;
}

interface LearnerProfilePayload {
  version: number;
  updated_at: string;
  weak_points: WeakPointRow[];
  preferences: {
    explanation_style: string | null;
    language: string | null;
    pace: string | null;
    others: string[];
  };
  goals: { goal: string; deadline: string | null }[];
  recent_status: string | null;
}

const EMPTY_FORM: ProfileFormState = {
  explanation_style: '',
  language: '',
  pace: '',
  others: [],
  weak_points: [],
  goals: [],
  recent_status: '',
};

function payloadToForm(profile: LearnerProfilePayload): ProfileFormState {
  return {
    explanation_style: profile.preferences.explanation_style ?? '',
    language: profile.preferences.language ?? '',
    pace: profile.preferences.pace ?? '',
    others: [...(profile.preferences.others ?? [])],
    weak_points: (profile.weak_points ?? []).map((wp) => ({
      subject: wp.subject ?? '',
      knowledge_point: wp.knowledge_point ?? '',
      error_pattern: wp.error_pattern ?? '',
      evidence_count: wp.evidence_count ?? 0,
      last_seen: wp.last_seen ?? null,
      source: wp.source ?? null,
    })),
    goals: (profile.goals ?? []).map((g) => ({
      goal: g.goal ?? '',
      deadline: g.deadline ?? '',
    })),
    recent_status: profile.recent_status ?? '',
  };
}

export function LearnerProfileSection() {
  const { t, i18n } = useTranslation(['settings', 'common']);
  const [form, setForm] = useState<ProfileFormState>(EMPTY_FORM);
  const [version, setVersion] = useState(0);
  const [updatedAt, setUpdatedAt] = useState('');
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [aiBusy, setAiBusy] = useState(false);
  const [newOther, setNewOther] = useState('');

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const resp = await invoke<{ found: boolean; profile?: LearnerProfilePayload }>(
        'learner_profile_get',
      );
      if (resp.found && resp.profile) {
        setForm(payloadToForm(resp.profile));
        setVersion(resp.profile.version ?? 0);
        setUpdatedAt(resp.profile.updated_at ?? '');
      } else {
        setForm(EMPTY_FORM);
        setVersion(0);
        setUpdatedAt('');
      }
    } catch (e) {
      showGlobalNotification('error', getErrorMessage(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const handleSave = async () => {
    setSaving(true);
    try {
      const payload: LearnerProfilePayload = {
        version,
        updated_at: updatedAt,
        preferences: {
          explanation_style: form.explanation_style.trim() || null,
          language: form.language.trim() || null,
          pace: form.pace.trim() || null,
          others: form.others.map((o) => o.trim()).filter(Boolean),
        },
        weak_points: form.weak_points
          .map((wp) => ({
            ...wp,
            subject: wp.subject.trim(),
            knowledge_point: wp.knowledge_point.trim(),
            error_pattern: wp.error_pattern.trim(),
          }))
          .filter((wp) => wp.subject && wp.knowledge_point),
        goals: form.goals
          .map((g) => ({ goal: g.goal.trim(), deadline: g.deadline.trim() || null }))
          .filter((g) => g.goal),
        recent_status: form.recent_status.trim() || null,
      };
      await invoke('learner_profile_save', { profile: payload });
      showGlobalNotification('success', t('settings:learner_profile.save_success'));
      await refresh();
    } catch (e) {
      showGlobalNotification('error', getErrorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  const handleAiAssist = async () => {
    setAiBusy(true);
    try {
      const sessionId = await ensureActiveChatSession();
      if (!sessionId) {
        showGlobalNotification('warning', t('settings:learner_profile.ai_no_session'));
        return;
      }
      queueChatInputText(sessionId, t('settings:learner_profile.ai_prompt'));
      dispatchAppEvent(APP_EVENTS.NAVIGATE_TO_TAB, { tabName: 'chat-v2' });
    } catch (e) {
      showGlobalNotification('error', getErrorMessage(e));
    } finally {
      setAiBusy(false);
    }
  };

  const locale = i18n.language || 'zh-CN';
  const updateForm = (patch: Partial<ProfileFormState>) => setForm((prev) => ({ ...prev, ...patch }));

  return (
    <SettingsGroup
      title={t('settings:learner_profile.title')}
      description={t('settings:learner_profile.description')}
      actions={
        <div className="flex items-center gap-2">
          {updatedAt ? (
            <span className="text-xs text-muted-foreground">
              {t('settings:learner_profile.updated_at')}{' '}
              {(() => {
                try {
                  return new Date(updatedAt).toLocaleString(locale, { hour12: false });
                } catch {
                  return updatedAt;
                }
              })()}
            </span>
          ) : null}
          <DsButton
            variant="outline"
            size="sm"
            disabled={aiBusy || loading}
            onClick={() => void handleAiAssist()}
          >
            <Sparkle className="h-4 w-4" weight="duotone" />
            {t('settings:learner_profile.ai_assist')}
          </DsButton>
        </div>
      }
    >
      {loading ? (
        <div className="px-1 py-6 text-sm text-muted-foreground">{t('settings:loading')}</div>
      ) : (
        <div className="flex flex-col gap-5 px-1">
          {/* 学习偏好 */}
          <div className="flex flex-col gap-2">
            <Label className="text-sm font-medium">{t('settings:learner_profile.preferences')}</Label>
            <div className="grid gap-2 md:grid-cols-3">
              <Input
                value={form.explanation_style}
                onChange={(e) => updateForm({ explanation_style: e.target.value })}
                placeholder={t('settings:learner_profile.style_placeholder')}
                maxLength={60}
                disabled={saving}
              />
              <Input
                value={form.language}
                onChange={(e) => updateForm({ language: e.target.value })}
                placeholder={t('settings:learner_profile.language_placeholder')}
                maxLength={30}
                disabled={saving}
              />
              <Input
                value={form.pace}
                onChange={(e) => updateForm({ pace: e.target.value })}
                placeholder={t('settings:learner_profile.pace_placeholder')}
                maxLength={60}
                disabled={saving}
              />
            </div>
            {form.others.length > 0 && (
              <div className="flex flex-wrap gap-1.5">
                {form.others.map((other, idx) => (
                  <Badge key={`${other}-${idx}`} variant="secondary" className="gap-1">
                    {other}
                    {/* eslint-disable-next-line ds-components/no-native-button -- 徽章 chip 的内联移除钮，DsButton 形态过大 */}
                    <button
                      type="button"
                      aria-label={t('common:delete')}
                      className="ml-0.5 inline-flex items-center rounded-full p-0.5 hover:bg-accent"
                      onClick={() =>
                        updateForm({ others: form.others.filter((_, i) => i !== idx) })
                      }
                    >
                      <X className="h-3 w-3" weight="bold" />
                    </button>
                  </Badge>
                ))}
              </div>
            )}
            <div className="flex items-center gap-2">
              <Input
                value={newOther}
                onChange={(e) => setNewOther(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' && newOther.trim()) {
                    updateForm({ others: [...form.others, newOther.trim()] });
                    setNewOther('');
                  }
                }}
                placeholder={t('settings:learner_profile.other_placeholder')}
                className="h-8 w-56"
                maxLength={60}
                disabled={saving}
              />
              <DsButton
                variant="ghost"
                size="sm"
                disabled={!newOther.trim() || saving}
                onClick={() => {
                  updateForm({ others: [...form.others, newOther.trim()] });
                  setNewOther('');
                }}
              >
                <Plus className="h-4 w-4" weight="bold" />
                {t('common:add')}
              </DsButton>
            </div>
          </div>

          {/* 学习目标 */}
          <div className="flex flex-col gap-2">
            <Label className="text-sm font-medium">{t('settings:learner_profile.goals')}</Label>
            {form.goals.map((g, idx) => (
              <div key={`goal-${idx}`} className="flex items-center gap-2">
                <Input
                  value={g.goal}
                  onChange={(e) => {
                    const goals = [...form.goals];
                    goals[idx] = { ...goals[idx], goal: e.target.value };
                    updateForm({ goals });
                  }}
                  placeholder={t('settings:learner_profile.goal_placeholder')}
                  className="h-8 flex-1"
                  maxLength={80}
                  disabled={saving}
                />
                <Input
                  type="date"
                  value={g.deadline}
                  onChange={(e) => {
                    const goals = [...form.goals];
                    goals[idx] = { ...goals[idx], deadline: e.target.value };
                    updateForm({ goals });
                  }}
                  className="h-8 w-40"
                  disabled={saving}
                />
                <DsButton
                  variant="ghost"
                  size="sm"
                  aria-label={t('common:delete')}
                  className="text-destructive hover:text-destructive"
                  disabled={saving}
                  onClick={() => updateForm({ goals: form.goals.filter((_, i) => i !== idx) })}
                >
                  <Trash className="h-4 w-4" />
                </DsButton>
              </div>
            ))}
            <DsButton
              variant="ghost"
              size="sm"
              className="self-start"
              disabled={saving}
              onClick={() => updateForm({ goals: [...form.goals, { goal: '', deadline: '' }] })}
            >
              <Plus className="h-4 w-4" weight="bold" />
              {t('settings:learner_profile.add_goal')}
            </DsButton>
          </div>

          {/* 薄弱知识点 */}
          <div className="flex flex-col gap-2">
            <Label className="text-sm font-medium">
              {t('settings:learner_profile.weak_points')}
              <span className="ml-2 text-xs font-normal text-muted-foreground">
                {t('settings:learner_profile.weak_points_hint')}
              </span>
            </Label>
            {form.weak_points.map((wp, idx) => (
              <div key={`wp-${idx}`} className="flex items-center gap-2">
                <Input
                  value={wp.subject}
                  onChange={(e) => {
                    const weak_points = [...form.weak_points];
                    weak_points[idx] = { ...weak_points[idx], subject: e.target.value };
                    updateForm({ weak_points });
                  }}
                  placeholder={t('settings:learner_profile.subject_placeholder')}
                  className="h-8 w-28"
                  maxLength={20}
                  disabled={saving}
                />
                <Input
                  value={wp.knowledge_point}
                  onChange={(e) => {
                    const weak_points = [...form.weak_points];
                    weak_points[idx] = { ...weak_points[idx], knowledge_point: e.target.value };
                    updateForm({ weak_points });
                  }}
                  placeholder={t('settings:learner_profile.point_placeholder')}
                  className="h-8 w-40"
                  maxLength={40}
                  disabled={saving}
                />
                <Input
                  value={wp.error_pattern}
                  onChange={(e) => {
                    const weak_points = [...form.weak_points];
                    weak_points[idx] = { ...weak_points[idx], error_pattern: e.target.value };
                    updateForm({ weak_points });
                  }}
                  placeholder={t('settings:learner_profile.pattern_placeholder')}
                  className="h-8 flex-1"
                  maxLength={60}
                  disabled={saving}
                />
                {wp.evidence_count > 0 && (
                  <Badge variant="secondary" className="shrink-0">
                    ×{wp.evidence_count}
                  </Badge>
                )}
                <DsButton
                  variant="ghost"
                  size="sm"
                  aria-label={t('common:delete')}
                  className="text-destructive hover:text-destructive"
                  disabled={saving}
                  onClick={() =>
                    updateForm({ weak_points: form.weak_points.filter((_, i) => i !== idx) })
                  }
                >
                  <Trash className="h-4 w-4" />
                </DsButton>
              </div>
            ))}
            <DsButton
              variant="ghost"
              size="sm"
              className="self-start"
              disabled={saving}
              onClick={() =>
                updateForm({
                  weak_points: [
                    ...form.weak_points,
                    {
                      subject: '',
                      knowledge_point: '',
                      error_pattern: '',
                      evidence_count: 0,
                      last_seen: null,
                      source: null,
                    },
                  ],
                })
              }
            >
              <Plus className="h-4 w-4" weight="bold" />
              {t('settings:learner_profile.add_weak_point')}
            </DsButton>
          </div>

          {/* 近期状态 */}
          <div className="flex flex-col gap-2">
            <Label className="text-sm font-medium">{t('settings:learner_profile.recent_status')}</Label>
            <Textarea
              value={form.recent_status}
              onChange={(e) => updateForm({ recent_status: e.target.value })}
              placeholder={t('settings:learner_profile.status_placeholder')}
              rows={3}
              maxLength={400}
              disabled={saving}
            />
          </div>

          <DsButton
            variant="default"
            size="sm"
            className="self-start"
            disabled={saving || loading}
            onClick={() => void handleSave()}
          >
            {t('common:save')}
          </DsButton>
        </div>
      )}
    </SettingsGroup>
  );
}

export default LearnerProfileSection;
