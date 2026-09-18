{{/*
Common labels.
*/}}
{{- define "kata-device-provisioner.labels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{/*
A CC mode, validated here so a typo fails at `helm install` rather than on the
first node the dispatcher reaches.

YAML 1.1 reads bare on/off/yes/no as booleans, so `ccMode: on` arrives as true.
Both spell the same intent, so take it rather than making the user quote it.
*/}}
{{- define "kata-device-provisioner.normalizeMode" -}}
{{- $mode := .mode | toString -}}
{{- if eq $mode "true" -}}
{{- $mode = "on" -}}
{{- else if eq $mode "false" -}}
{{- $mode = "off" -}}
{{- end -}}
{{- if not (has $mode (list "off" "on" "devtools" "ppcie")) -}}
{{- fail (printf "%s must be one of off, on, devtools, ppcie (got %q)" .field $mode) -}}
{{- end -}}
{{- $mode -}}
{{- end -}}

{{- define "kata-device-provisioner.ccMode" -}}
{{- include "kata-device-provisioner.normalizeMode" (dict "mode" .Values.ccMode "field" "ccMode") -}}
{{- end -}}

{{/*
The modes reachable through the per-node override label, comma-joined because a
template cannot return a list. Callers splitList it back.
*/}}
{{- define "kata-device-provisioner.overrideModes" -}}
{{- $modes := list -}}
{{- range $mode := .Values.ccModeOverrides | default list -}}
{{- $modes = append $modes (include "kata-device-provisioner.normalizeMode" (dict "mode" $mode "field" "ccModeOverrides")) -}}
{{- end -}}
{{- if and $modes .Values.job.nodes -}}
{{- fail "ccModeOverrides selects nodes by label, but job.nodes names them outright: the two cannot both decide which nodes get which mode. Drop one." -}}
{{- end -}}
{{- join "," ($modes | uniq) -}}
{{- end -}}

{{/*
Every mode this release has to be able to run, so the templates ConfigMap can
hold one per-node Job per mode.
*/}}
{{- define "kata-device-provisioner.allModes" -}}
{{- $modes := list (include "kata-device-provisioner.ccMode" .) -}}
{{- $modes = concat $modes (include "kata-device-provisioner.overrideModes" . | splitList "," | compact) -}}
{{- join "," ($modes | uniq) -}}
{{- end -}}

{{/*
Image references. Both accept reference:tag and reference@sha256:digest.
*/}}
{{- define "kata-device-provisioner.image" -}}
{{- include "kata-device-provisioner.imageRef" (dict "image" .Values.image "key" "image") -}}
{{- end -}}

{{- define "kata-device-provisioner.dispatcherImage" -}}
{{- include "kata-device-provisioner.imageRef" (dict "image" .Values.job.dispatcherImage "key" "job.dispatcherImage") -}}
{{- end -}}

{{- define "kata-device-provisioner.imageRef" -}}
{{- $ref := .image.reference -}}
{{- $tag := .image.tag | toString -}}
{{- if contains "@" $ref -}}
{{- $ref -}}
{{- else if eq $tag "" -}}
{{- fail (printf "%s.tag is required when %s.reference is not a digest" .key .key) -}}
{{- else -}}
{{- printf "%s:%s" $ref $tag -}}
{{- end -}}
{{- end -}}

{{- define "kata-device-provisioner.dispatcherServiceAccountName" -}}
{{ .Chart.Name }}-dispatcher
{{- end -}}

{{/*
`nodeSelector` as the label selector the dispatcher lists nodes with, plus the
run's own `extra` requirement. Empty means every node, which the dispatcher
takes as "no filter".
*/}}
{{- define "kata-device-provisioner.nodeSelector" -}}
{{- $terms := list -}}
{{- range $key, $value := .root.Values.nodeSelector -}}
{{- $terms = append $terms (printf "%s=%s" $key ($value | toString)) -}}
{{- end -}}
{{- range $expr := .root.Values.nodeSelectorExpressions | default list -}}
{{- $terms = append $terms ($expr | toString) -}}
{{- end -}}
{{- with .extra -}}
{{- $terms = append $terms . -}}
{{- end -}}
{{- join "," $terms -}}
{{- end -}}

{{/*
Per-node Job, rendered into the templates ConfigMap once per mode and cloned by
the dispatcher for every selected node, which injects metadata.name and
spec.template.spec.nodeName.

`stage` is "provision" (apply, and then `ccMode` applies) or "unprovision"
(uninstall).
*/}}
{{- define "kata-device-provisioner.perNodeJob" -}}
{{- $root := .root -}}
{{- $stage := .stage -}}
{{- if lt (int $root.Values.job.ttlSecondsAfterFinished) 60 -}}
{{- fail (printf "job.ttlSecondsAfterFinished is %v, too short for the dispatcher to observe a per-node Job finishing: the Job is deleted before it is next polled and its node is reported as failed even though its run succeeded. Use 60 or more." $root.Values.job.ttlSecondsAfterFinished) -}}
{{- end -}}
apiVersion: batch/v1
kind: Job
metadata:
  labels:
{{- include "kata-device-provisioner.labels" $root | nindent 4 }}
    kata-device-provisioner/stage: {{ $stage }}
{{- if eq $stage "provision" }}
    kata-device-provisioner/cc-mode: {{ .ccMode | quote }}
{{- end }}
spec:
  backoffLimit: {{ $root.Values.job.backoffLimit }}
  ttlSecondsAfterFinished: {{ $root.Values.job.ttlSecondsAfterFinished }}
  activeDeadlineSeconds: {{ $root.Values.job.activeDeadlineSeconds }}
  template:
    metadata:
      labels:
{{- with $root.Values.podLabels }}
{{- toYaml . | nindent 8 }}
{{- end }}
{{- include "kata-device-provisioner.labels" $root | nindent 8 }}
        kata-device-provisioner/stage: {{ $stage }}
{{- with $root.Values.podAnnotations }}
      annotations:
{{- toYaml . | nindent 8 }}
{{- end }}
    spec:
{{- with $root.Values.imagePullSecrets }}
      imagePullSecrets:
{{- toYaml . | nindent 8 }}
{{- end }}
      {{- /* This pod mutates the hardware of a node that also runs untrusted
             workloads. The dispatcher does every API call there is, so the pod
             needs no identity and must not be handed the default one. */}}
      automountServiceAccountToken: false
      restartPolicy: Never
      {{- /* The idle check looks for a VM holding a GPU open, by reading the
             open files of every process. A private PID namespace shows only
             this pod, making every node look idle and every reset safe. */}}
      hostPID: true
{{- if eq $stage "unprovision" }}
      {{- /* Uninstall has to reach every node the install ever touched,
             whatever it has been tainted with since. */}}
      tolerations:
        - operator: Exists
{{- else if $root.Values.job.nodes }}
      {{- /* Naming a node is an explicit override of admission. nodeName gets
             the pod past the scheduler, but a NoExecute taint can still evict
             it mid-reset. */}}
      tolerations:
        - operator: Exists
{{- else }}
      tolerations:
        - key: node.kubernetes.io/not-ready
          operator: Exists
          effect: NoExecute
        - key: node.kubernetes.io/unreachable
          operator: Exists
          effect: NoExecute
        - key: node.kubernetes.io/unschedulable
          operator: Exists
          effect: NoSchedule
{{- with $root.Values.tolerations }}
{{- toYaml . | nindent 8 }}
{{- end }}
{{- end }}
{{- with $root.Values.priorityClassName }}
      priorityClassName: {{ . | quote }}
{{- end }}
      containers:
        - name: {{ $stage }}
          image: {{ include "kata-device-provisioner.image" $root }}
          imagePullPolicy: {{ $root.Values.imagePullPolicy }}
          command:
            - /kata-device-provisioner
{{- if eq $stage "unprovision" }}
            - uninstall
{{- else }}
            - apply
            - "--mode={{ .ccMode }}"
{{- end }}
            - "--host-root=/host"
          securityContext:
            {{- /* Mapping BAR0 to reach the GPU's firmware needs CAP_SYS_RAWIO,
                   running the host's modprobe needs CAP_SYS_CHROOT, and sysfs
                   has to be writable. Nothing here is a subset worth naming. */}}
            privileged: true
            readOnlyRootFilesystem: true
{{- with $root.Values.resources }}
          resources:
{{- toYaml . | nindent 12 }}
{{- end }}
          volumeMounts:
            - name: sys
              mountPath: /sys
            - name: dev-vfio
              mountPath: /dev/vfio
            - name: host-root
              mountPath: /host
      volumes:
        {{- /* Writable: driver_override and drivers_probe are how a device is
               bound, and resource0 is how its CC mode is read. */}}
        - name: sys
          hostPath:
            path: /sys
            type: Directory
        {{- /* Created if absent: a node that has never loaded vfio-pci has no
               /dev/vfio yet, and this run is what loads it. */}}
        - name: dev-vfio
          hostPath:
            path: /dev/vfio
            type: DirectoryOrCreate
        {{- /* The whole node filesystem, writable, because the durable half of
               a run is the node's own modprobe configuration, and because the
               modprobe that loads vfio-pci is the host's own. */}}
        - name: host-root
          hostPath:
            path: /
            type: Directory
{{- end -}}

{{/*
The dispatcher flags that are the same for both stages.
*/}}
{{- define "kata-device-provisioner.dispatcherCommonFlags" -}}
{{- $root := .root -}}
- "--owner-job-name={{ .dispatcherName }}"
- "--parallelism={{ $root.Values.job.parallelism }}"
{{- /* The dispatcher's own bookkeeping, namespaced so a cluster running
       kata-deploy's dispatcher too cannot mistake these Jobs for its own. */}}
- "--tracking-label-prefix=kata-device-provisioner-dispatcher"
- "--instance-label-prefix=kata-device-provisioner.katacontainers.io"
- "--node-label-key={{ $root.Values.ccModeStateLabel }}"
{{- end -}}

{{/*
Where the dispatcher pods themselves run. Falls back to the per-node Jobs'
tolerations so a single-node cluster, where every node is tainted, still
schedules it.
*/}}
{{- define "kata-device-provisioner.dispatcherPlacement" -}}
{{- $root := . -}}
{{- with $root.Values.job.dispatcherNodeSelector }}
nodeSelector:
{{- toYaml . | nindent 2 }}
{{- end }}
{{- $tolerations := $root.Values.job.dispatcherTolerations | default $root.Values.tolerations }}
{{- with $tolerations }}
tolerations:
{{- toYaml . | nindent 2 }}
{{- end }}
{{- end -}}
