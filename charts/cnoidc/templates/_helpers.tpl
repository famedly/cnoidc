{{/*
SPDX-FileCopyrightText: 2026 Famedly GmbH (info@famedly.com)

SPDX-License-Identifier: AGPL-3.0-or-later
*/}}

{{/*
Expand the name of the chart.
*/}}
{{- define "cnoidc.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name (63 chars max, DNS naming spec).
*/}}
{{- define "cnoidc.fullname" -}}
{{- if contains .Chart.Name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}

{{- define "cnoidc.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "cnoidc.labels" -}}
helm.sh/chart: {{ include "cnoidc.chart" . }}
{{ include "cnoidc.selectorLabels" . }}
app.kubernetes.io/version: {{ .Values.image.tag | default .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "cnoidc.selectorLabels" -}}
app.kubernetes.io/name: {{ include "cnoidc.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "cnoidc.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "cnoidc.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{- define "cnoidc.image" -}}
{{- printf "%s:%s" .Values.image.repository (.Values.image.tag | default .Chart.AppVersion) }}
{{- end }}

{{/*
Rules the operator needs on its custom resources and the objects it writes,
shared by the ClusterRole and the namespaced Role.
*/}}
{{- define "cnoidc.rules" -}}
- apiGroups: ["cnoidc.famedly.com"]
  resources: ["oidcapplications", "projectroles"]
  verbs: ["get", "list", "watch", "update", "patch"]
- apiGroups: ["cnoidc.famedly.com"]
  resources: ["oidcapplications/status", "projectroles/status"]
  verbs: ["get", "update", "patch"]
- apiGroups: [""]
  resources: ["configmaps", "secrets"]
  verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
- apiGroups: [""]
  resources: ["events"]
  verbs: ["create", "patch"]
{{- end }}
