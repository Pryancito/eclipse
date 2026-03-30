# Mejoras posibles a la librería IPC (`eclipse_ipc`)

Resumen de mejoras sugeridas tras revisar `channel`, `protocol`, `services` y `types`.

---

## 1. Rendimiento y uso de memoria

### 1.1 Buffer del slow path en el struct
**Problema:** En `IpcChannel::recv()` se crea un buffer local de 256 bytes en cada llamada al slow path:
```rust
let mut buf = [0u8; SLOW_BUF];
```
**Mejora:** Mover el buffer a `IpcChannel` (ej. `slow_buf: [u8; SLOW_BUF]`). Así se reutiliza el mismo buffer, se reduce presión en el stack y se evita rellenar 256 bytes en cada `recv()` cuando solo se usa el fast path.

### 1.2 Evitar probar slow path si `receive_fast` ya devolvió algo
**Problema:** Si `receive_fast()` devuelve datos pero `parse_fast()` falla (tamaño inesperado), el código hace igualmente `receive(&mut buf)`, que puede consumir el siguiente mensaje.
**Mejora:** Si hay datos en fast path pero no se parsean, decidir si: (a) no llamar a `receive()` y devolver `None` (o un `Raw` con esos 24 bytes), o (b) documentar que “mensaje pequeño no reconocido” se descarta y el siguiente mensaje se lee por slow path. Así se evita confusión y posible pérdida de mensajes.

---

## 2. API y ergonomía

### 2.1 Envío tipado
**Problema:** Solo existe `send_raw(dest_pid, msg_type, data)`. Quien envía SideWind o eventos tiene que serializar a mano.
**Mejora:** Añadir helpers que usen el mismo esquema que el parser, por ejemplo:
- `send_sidewind(dest_pid, &SideWindMessage) -> bool`
- Reutilizar `EclipseEncode` (o un trait similar para mensajes >24 bytes) para que los tipos que ya implementan encode puedan enviarse con una sola llamada.

### 2.2 Unificar construcción de mensajes de control
**Problema:** "SUBS"+pid, "INPT"+pid, "GET_INPUT_PID" se construyen en `channel.rs` y en `services.rs` por duplicado.
**Mejora:** Definir en un solo módulo (p. ej. `protocol` o `types`) constantes o funciones `fn build_subscribe(self_pid: u32) -> [u8; 8]`, `fn build_input_pid_response(pid: u32) -> [u8; 8]`, y usarlas desde `channel` y `services`. Así no se repiten los magic bytes.

### 2.3 Recibir con timeout o “recv_blocking”
**Problema:** Solo hay `recv()` no bloqueante. Para arranque o flujos que necesitan esperar (p. ej. `query_input_service_pid`), el usuario implementa bucles con `yield_cpu()`.
**Mejora:** Opcionalmente ofrecer `recv_blocking_for(&mut self, max_attempts: u32)` o similar que internamente haga el bucle recv + yield, para centralizar la política (reintentos, backoff, etc.).

---

## 3. Extensibilidad

### 3.1 Registro de tipos de mensaje
**Problema:** `parse_slow` tiene todos los tags fijos ("SWND", "SUBS", "INPT", "GET_INPUT_PID"). Añadir un nuevo tipo implica tocar el core de la librería.
**Mejora:** Introducir un trait `DecodeMessage` (o similar) que, dado `&[u8]` y `from: u32`, devuelva `Option<EclipseMessage>` o un enum genérico. El parser actual puede ser la implementación por defecto; en el futuro se podría permitir registrar parsers adicionales o un “catch-all” que rellene `Raw`. Mantener `no_std` y sin heap.

### 3.2 Coherencia de `EclipseMessage::Raw` con `SLOW_BUF`
**Problema:** `Raw { data: [u8; 256], len, from }` está fijado a 256; `SLOW_BUF` también es 256 pero no hay una única constante compartida.
**Mejora:** Definir algo como `pub const MAX_MSG_LEN: usize = 256` en un sitio (p. ej. `types` o `channel`) y usar ese valor tanto para el buffer como para el tamaño de `Raw.data`, para que no se desincronice.

---

## 4. Consistencia y mantenibilidad

### 4.1 Un solo origen para `MSG_TYPE_INPUT`
**Problema:** En `protocol.rs`, `InputEvent::msg_type()` devuelve `0x00000040` hardcodeado; en `services.rs` está `MSG_TYPE_INPUT = 0x00000040`. Si se cambia uno, el otro puede quedar desincronizado.
**Mejora:** En `EclipseEncode for InputEvent`, usar `crate::services::MSG_TYPE_INPUT` (o reexportar la constante desde un módulo común) en lugar del literal.

### 4.2 Prelude y constantes
**Problema:** Quien quiera enviar con un `msg_type` concreto tiene que importar `services::MSG_TYPE_*` aparte.
**Mejora:** Incluir en el prelude las constantes `MSG_TYPE_*` más usadas (al menos `MSG_TYPE_INPUT`, `MSG_TYPE_GRAPHICS`) para que la API de envío y la de constantes estén en el mismo sitio.

### 4.3 Documentar orden fast → slow
**Mejora:** En la doc de `recv()` (y en el módulo) dejar explícito: “Siempre se intenta primero el fast path; si no hay mensaje pequeño, se intenta el slow path. Si el mensaje pequeño no se reconoce, se descarta y no se lee el siguiente en esta llamada” (o el comportamiento que se elija en 1.2).

---

## 5. Descubrimiento de servicios

### 5.1 Configurar reintentos en `query_input_service_pid`
**Problema:** El límite de 10_000 intentos está fijo; en máquinas lentas puede no bastar, en rápidas es excesivo.
**Mejora:** Parámetro opcional `max_attempts: Option<u32>` (por defecto 10_000) o una constante pública `DEFAULT_INPUT_QUERY_ATTEMPTS` que el usuario pueda sobrescribir o usar en tests.

### 5.2 Eliminar duplicación con `send_subscribe`
**Problema:** `IpcChannel::send_subscribe` y `services::subscribe_to_input` construyen el mismo mensaje "SUBS"+pid.
**Mejora:** Que ambos usen la misma función de construcción (ver 2.2) o que `subscribe_to_input` llame a `IpcChannel::send_subscribe` (o a una función interna compartida) para no duplicar lógica.

---

## 6. Tests y robustez

### 6.1 Tests unitarios para parsing
**Mejora:** Añadir tests (compatibles con `no_std`) para:
- `parse_fast`: buffer de 24 bytes con un `InputEvent` válido; buffers de tamaño distinto o basura → `None`.
- `parse_slow`: "SWND"+SideWindMessage válido, "SUBS"+pid, "INPT"+pid, "GET_INPUT_PID", mensaje desconocido → `Raw`.

Así los cambios en tags o tamaños no rompen el parser sin que se note.
**Nota:** El crate es `no_std` y el target es `x86_64-unknown-linux-musl`, por lo que el test runner estándar no está disponible. Los tests se pueden añadir cuando exista un build para host o un test runner no_std.

### 6.2 Validación y seguridad
**Mejora:** Antes de cada `read_unaligned` o `copy_from_slice` en `parse_slow`, comprobar explícitamente `len >= size_of::<T>()` (y que el tag coincide cuando aplique). Ya se hace en parte; documentar que el buffer de `receive()` puede no estar alineado y que por eso se usa `read_unaligned`.

---

## Priorización sugerida

| Prioridad | Mejora | Esfuerzo | Impacto |
|-----------|--------|----------|---------|
| Alta      | Buffer en struct (1.1) | Bajo | Menos stack, mismo comportamiento |
| Alta      | Un solo origen MSG_TYPE / construcción SUBS/INPT (2.2, 4.1) | Bajo | Menos bugs, código más claro |
| Media     | Envío tipado SideWind (2.1) | Medio | Mejor API para compositor y clientes |
| Media     | `MAX_MSG_LEN` único (3.2) | Bajo | Mantenibilidad |
| Media     | Tests de parsing (6.1) | Medio | Regresiones |
| Baja      | Recv con timeout / recv_blocking (2.3) | Bajo | Comodidad en arranque |
| Baja      | Reintentos configurables (5.1) | Bajo | Flexibilidad |

Si quieres, el siguiente paso puede ser implementar las de prioridad alta (buffer en struct + unificar constantes y construcción de mensajes) en el código actual.
