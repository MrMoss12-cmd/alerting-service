#!/usr/bin/env bash
# simulate_alert.sh
# Script para simular la generación y envío de alertas al microservicio
# Autor: Equipo de QA / DevOps
# Fecha: 2025-08-12

#######################################
# CONFIGURACIÓN PREDETERMINADA
#######################################
API_URL="${API_URL:-http://localhost:8080/api/v1/alerts}"  # Cambia a tu endpoint real
AUTH_TOKEN="${AUTH_TOKEN:-changeme-token}"
DEFAULT_SEVERITY="medium"
DEFAULT_TENANT="tenant_a"
DEFAULT_ORIGIN="simulator"
DEFAULT_CONTENT="This is a simulated alert"
BATCH_SIZE=1
SLEEP_BETWEEN=1

#######################################
# FUNCIÓN: Mostrar ayuda
#######################################
usage() {
  echo "Uso: $0 [-s severidad] [-t tenant] [-o origen] [-c contenido] [-n cantidad] [-d delay] [-a api_url] [-k token]"
  echo "Ejemplo:"
  echo "  $0 -s high -t tenant_b -o sensor1 -c 'Overheating detected' -n 5 -d 2"
  exit 1
}

#######################################
# FUNCIÓN: Validar severidad
#######################################
validate_severity() {
  case "$1" in
    low|medium|high|critical) return 0 ;;
    *) echo "ERROR: Severidad inválida: $1. Valores permitidos: low, medium, high, critical" >&2; exit 1 ;;
  esac
}

#######################################
# PARSEAR PARÁMETROS
#######################################
while getopts ":s:t:o:c:n:d:a:k:h" opt; do
  case $opt in
    s) SEVERITY="$OPTARG" ;;
    t) TENANT="$OPTARG" ;;
    o) ORIGIN="$OPTARG" ;;
    c) CONTENT="$OPTARG" ;;
    n) BATCH_SIZE="$OPTARG" ;;
    d) SLEEP_BETWEEN="$OPTARG" ;;
    a) API_URL="$OPTARG" ;;
    k) AUTH_TOKEN="$OPTARG" ;;
    h) usage ;;
    *) echo "Opción inválida: -$OPTARG" >&2; usage ;;
  esac
done

# Valores por defecto si no se pasan parámetros
SEVERITY="${SEVERITY:-$DEFAULT_SEVERITY}"
TENANT="${TENANT:-$DEFAULT_TENANT}"
ORIGIN="${ORIGIN:-$DEFAULT_ORIGIN}"
CONTENT="${CONTENT:-$DEFAULT_CONTENT}"

#######################################
# VALIDACIONES
#######################################
validate_severity "$SEVERITY"

if [[ -z "$AUTH_TOKEN" || "$AUTH_TOKEN" == "changeme-token" ]]; then
  echo "ADVERTENCIA: No se proporcionó un token real de autenticación" >&2
fi

if ! [[ "$BATCH_SIZE" =~ ^[0-9]+$ ]] || [ "$BATCH_SIZE" -lt 1 ]; then
  echo "ERROR: La cantidad (-n) debe ser un número entero mayor a 0" >&2
  exit 1
fi

if ! [[ "$SLEEP_BETWEEN" =~ ^[0-9]+$ ]]; then
  echo "ERROR: El delay (-d) debe ser un número entero en segundos" >&2
  exit 1
fi

#######################################
# FUNCIÓN: Enviar alerta
#######################################
send_alert() {
  local severity="$1"
  local tenant="$2"
  local origin="$3"
  local content="$4"

  local payload
  payload=$(jq -n \
    --arg sev "$severity" \
    --arg ten "$tenant" \
    --arg ori "$origin" \
    --arg con "$content" \
    '{
      severity: $sev,
      tenant: $ten,
      origin: $ori,
      content: $con,
      timestamp: (now | todate)
    }'
  )

  local response
  response=$(curl -s -o /tmp/alert_response.json -w "%{http_code}" \
    -X POST "$API_URL" \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer $AUTH_TOKEN" \
    -d "$payload")

  # Registro
  if [[ "$response" == "200" || "$response" == "201" ]]; then
    echo "[OK] Alerta enviada a $API_URL (tenant=$tenant, severidad=$severity)"
  else
    echo "[ERROR] Código HTTP $response al enviar alerta (tenant=$tenant, severidad=$severity)"
    cat /tmp/alert_response.json >&2
  fi
}

#######################################
# SIMULACIÓN
#######################################
echo "=== Simulación de envío de $BATCH_SIZE alertas ==="
for ((i=1; i<=BATCH_SIZE; i++)); do
  send_alert "$SEVERITY" "$TENANT" "$ORIGIN" "$CONTENT"
  if [ "$i" -lt "$BATCH_SIZE" ]; then
    sleep "$SLEEP_BETWEEN"
  fi
done

echo "=== Simulación completada ==="
