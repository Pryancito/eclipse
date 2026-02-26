# Por dónde continuar – Eclipse OS

Opciones ordenadas por área. Lo que ya hicimos en IPC está marcado como ✅.

---

## A. Librería IPC (`eclipse_ipc`) – pendiente

| Qué | Esfuerzo | Notas |
|-----|----------|--------|
| ~~**1.2** Comportamiento cuando fast path no parsea~~ | ✅ | Hecho: no se llama a `receive()`; se devuelve `None`. Documentado en `recv()`. |
| ~~**2.3** `recv_blocking_for(max_attempts)`~~ | ✅ | Hecho: bucle recv + yield_cpu hasta mensaje o max_attempts. |
| ~~**4.2** Prelude con más constantes~~ | ✅ | Hecho: todos los `MSG_TYPE_*` exportados en el prelude. |
| ~~**4.3** Documentar orden fast → slow en `recv()`~~ | ✅ | Hecho: doc ampliada con orden y comportamiento cuando fast no parsea. |
| ~~**5.1** Reintentos configurables en `query_input_service_pid`~~ | ✅ | Hecho: `DEFAULT_INPUT_QUERY_ATTEMPTS` y `query_input_service_pid_with_attempts(max_attempts)`. |
| **6.1** Tests de parsing | Medio | Requiere build para host o test runner no_std (ahora no disponible para el target). |
| **3.1** Trait `DecodeMessage` / registro de tipos | Medio–Alto | Extensibilidad sin tocar el core al añadir mensajes. |

**Ya hecho:** 1.1 buffer en struct, 2.1 send_sidewind, 2.2 builders SUBS/INPT, 3.2 MAX_MSG_LEN, 4.1 MSG_TYPE_INPUT único, 5.2 subscribe usa builder, sidewind_sdk migrado a send_sidewind.

---

## B. Kernel

- **IPC/kernel** – Revisar si el lado kernel (syscalls, colas, fast path) está alineado con lo que asume `eclipse_ipc` (tamaños, msg_type, no bloqueo).
- **USB HID** (`eclipse_kernel/src/usb_hid.rs`) – Mejoras de robustez, manejo de errores, limpieza de dispositivos, o preparar envío de eventos al input_service vía IPC.
- **Syscalls** – Documentar o extender syscalls que usan las apps (send/receive, receive_fast, etc.).
- **Procesos / init** – Servicios que responden GET_INPUT_PID, GET_DISPLAY_PID; timeout o reintentos si algo no arranca.

---

## C. Apps / usuariospace

- **Compositor (smithay_app)** – Simplificar uso de IPC (ya usa eclipse_ipc), limpieza de warnings, o más manejo de eventos (Wayland/X11 si aplica).
- **demo_client / sidewind_sdk** – Ya usan send_sidewind; podrían usar más prelude de eclipse_ipc si se añaden helpers (recv_blocking_for, constantes).
- **input_service** – Asegurar que envía InputEvent por fast path cuando corresponda y que el formato coincide con `InputEvent` en libc.

---

## D. Infraestructura / calidad

- **Tests** – Tests de integración o host-only para eclipse_ipc (parsing, builders) cuando el target lo permita.
- **Docs** – README o doc en eclipse_ipc con ejemplo mínimo de uso (recv, send_sidewind, subscribe).
- **Warnings** – Ir limpiando warnings en libc, sidewind_sdk, smithay_app (unused imports, etc.) en pasadas dedicadas.

---

## Sugerencia de orden

1. **Rápido y estable:** 1.2 (comportamiento fast path no parseado) + 4.3 (documentar recv).  
2. **Útil para arranque:** 2.3 recv_blocking_for y/o 5.1 reintentos configurables.  
3. **Kernel:** Revisar/usar `usb_hid` o IPC en kernel si quieres enfocarte ahí.  
4. **Extensibilidad:** 3.1 DecodeMessage cuando quieras añadir más tipos de mensaje sin tocar el core.

Si dices en qué área quieres seguir (IPC, kernel, apps, docs), se puede bajar al detalle y proponer cambios concretos en código.
