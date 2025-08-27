#!/usr/bin/env bash
# replay_alerts.sh
# Script para re-ejecutar alertas previamente registradas.
# Permite filtros, control de velocidad y registro detallado.
# Compatible con ejecución local y CI/CD.
# Autor: Equipo Alerting-Service

set -euo pipefail

##############################################
# CONFIGURACIÓN Y VARIABLES
##############################################

API_URL="http://localhost:8080/api/alerts/replay"
AUTH_TOKEN="${AUTH_TOKEN:-}"  # Debe pasarse como variable de entorno
ALERTS_FILE="./alerts_history.json"
FILTER_TENANT=""
FILTER_DATE=""
SPEED_DELAY=0.5
DRY_RUN=false
LOG_FILE="./replay_alerts.log"

##############################################
# FUNCIONES
##############################################

print_usage() {
    echo "Uso: $0 [opciones]"
    echo "Opciones:"
    echo "  -f, --file <archivo>       Archivo JSON con alertas a reenviar (por defecto: $ALERTS_FILE)"
    echo "  -t, --tenant <id>          Filtrar por tenant ID"
    echo "  -d, --date <YYYY-MM-DD>    Filtrar por fecha mínima de alerta"
    echo "  -s, --speed <segundos>     Pausa entre envíos (default: $SPEED_DELAY)"
    echo "  --dry-run                  Mostrar alertas a reenviar sin enviarlas"
    echo "  -h, --help                 Mostrar ayuda"
    echo ""
    echo "Ejemplo:"
    echo "  $0 -f ./mis_alertas.json -t tenant123 -d 2025-08-01 -s 1"
}

check_requirements() {
    if ! command -v jq >/dev/null; then
        echo "ERROR: 'jq' es requerido para procesar JSON."
        exit 1
    fi
    if [[ -z "$AUTH_TOKEN" ]]; then
        echo "ERROR: Debes exportar AUTH_TOKEN antes de ejecutar."
        exit 1
    fi
}

filter_alerts() {
    local jq_filter="."
    [[ -n "$FILTER_TENANT" ]] && jq_filter="$jq_filter | select(.tenantId == \"$FILTER_TENANT\")"
    [[ -n "$FILTER_DATE" ]] && jq_filter="$jq_filter | select(.timestamp >= \"$FILTER_DATE\")"
    jq -c "$jq_filter" "$ALERTS_FILE"
}

send_alert() {
    local alert_json="$1"
    curl -s -o /dev/null -w "%{http_code}" \
        -X POST "$API_URL" \
        -H "Authorization: Bearer $AUTH_TOKEN" \
        -H "Content-Type: application/json" \
        -d "$alert_json"
}

log_action() {
    local msg="$1"
    echo "$(date '+%F %T') - $msg" >> "$LOG_FILE"
}

##############################################
# PARÁMETROS CLI
##############################################

while [[ $# -gt 0 ]]; do
    case $1 in
        -f|--file) ALERTS_FILE="$2"; shift 2;;
        -t|--tenant) FILTER_TENANT="$2"; shift 2;;
        -d|--date) FILTER_DATE="$2"; shift 2;;
        -s|--speed) SPEED_DELAY="$2"; shift 2;;
        --dry-run) DRY_RUN=true; shift;;
        -h|--help) print_usage; exit 0;;
        *) echo "Opción desconocida: $1"; print_usage; exit 1;;
    esac
done

##############################################
# EJECUCIÓN
##############################################

check_requirements

echo "== Reejecución de alertas iniciada =="
log_action "Iniciando replay de alertas desde archivo: $ALERTS_FILE"

alerts=$(filter_alerts)
count=$(echo "$alerts" | jq -s 'length')
echo "Se encontraron $count alertas para reenviar."

if $DRY_RUN; then
    echo "$alerts" | jq '.'
    log_action "DRY RUN - Sin envíos reales."
    exit 0
fi

echo "$alerts" | while read -r alert; do
    status_code=$(send_alert "$alert")
    if [[ "$status_code" -eq 200 ]]; then
        echo "✅ Alerta reenviada correctamente."
        log_action "OK: $alert"
    else
        echo "❌ Error al reenviar alerta (HTTP $status_code)"
        log_action "FAIL ($status_code): $alert"
    fi
    sleep "$SPEED_DELAY"
done

echo "== Replay completado =="
log_action "Replay completado exitosamente."
