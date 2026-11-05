# `WAIT_VBLANK` bloquea (resuelto) — y la restricción del `ioctl` síncrono que había detrás

[English](README-async-ioctl-vblank.md) · **Español**

Esto empezó como un traspaso para una refactorización. El bug está arreglado;
lo que sigue mereciendo la pena leer es *cómo*, porque la restricción que dio
forma al arreglo sigue ahí para el próximo ioctl que necesite esperar.

## El bug

La forma bloqueante de `DRM_IOCTL_WAIT_VBLANK` volvía al instante en vez de
esperar. Medido con `eclipse-bench --only gfx` en Eclipse bajo QEMU:

```
vblank interval            0.04 ms      <-- debería ser 16.7 a 60 Hz
vblank jitter (stddev)     0.02 ms
```

La forma por evento (`_DRM_VBLANK_EVENT`) siempre fue correcta — difiere a
través de la cola de temporizadores — y los page flips ya iban bien
acompasados a ~60 Hz. Solo la rama bloqueante estaba mal, así que un cliente
que acompasa sus frames con `drmWaitVBlank` (el Present de X, SDL, toolkits
antiguos) giraba al 100 % de CPU en vez de dormir.

## Por qué no podía simplemente dormir

`INode::io_control` es síncrono, y en este kernel no hay un yield síncrono —
un marco síncrono no puede devolver la CPU. Una implementación anterior
«bloqueaba» girando en vacío los 16,7 ms, lo que dejaba sin CPU a todas las
demás corrutinas de esa CPU y hacía que la máquina pareciera congelada. **No
vuelvas a meter un spin.**

## Qué se hizo

No la refactorización amplia que este documento recomendaba antes (`async fn
ioctl` en `FileLike`, propagada por todos los implementadores). La espera se
puso donde ya existe un contexto asíncrono:

- `sys_ioctl` pasó a ser `async fn` — el despachador de syscalls ya era
  asíncrono, así que es un `.await` en el punto de llamada y ningún cambio en
  `FileLike`.
- Antes de despachar, llama a `DrmDev::wait_vblank_sleep`, que resuelve la
  secuencia objetivo exactamente igual que la rama síncrona y duerme hasta el
  deadline de ese vblank. El contador sintético es una función pura del reloj
  (`drm::vblank_deadline_for_seq`), así que hay un deadline exacto hasta el
  que dormir y ningún sondeo.
- Después, la rama síncrona informa de la secuencia que de verdad se ha
  completado, porque el sueño fue antes. Su lógica no cambió.
- La intercepción depende de que el fd sea de verdad un `DrmDev`, no solo del
  número de petición: dormir es un efecto secundario y no se le puede imponer
  a un fd cualquiera al que le pasen ese número.

Dos detalles que resultaron importar:

- **Dormir en un bucle acotado, no una sola vez.** Un despertar que cae un
  nanosegundo antes del límite de la retícula deja el contador uno por
  debajo; la rama síncrona informa entonces `target - 1`, la siguiente
  petición relativa del llamador se resuelve a un vblank que acaba de pasar y
  vuelve al instante, y el frame siguiente espera un periodo entero. Esa
  alternancia se ve como jitter — 4,69 ms de desviación típica medidos con un
  solo sueño, 1,40 ms con el bucle de recomprobación.
- **Limitar la espera a 3 s**, como el `DRM_WAIT_ON(..., 3 * HZ, ...)` de
  Linux, para que un objetivo absoluto muy en el futuro no pueda aparcar un
  hilo para siempre.

## Resultado

```
                        antes       después
vblank interval         0.04 ms     16.5 ms     (60 Hz = 16.7)
vblank jitter (stddev)  0.02 ms      1.40 ms
flip period / vblank    n/a          1.00 x     <-- acompasado al refresco
flip rate               61.4 /s      60.8 /s    <-- el camino del flip intacto
```

El 1,4 ms de jitter que queda está medido bajo TCG, donde los despertares de
temporizador son imprecisos; no se ha vuelto a medir bajo KVM.

## Si otro ioctl necesita bloquear

La refactorización general sigue disponible y sigue siendo la respuesta más
limpia si aparece un segundo: `async fn ioctl` en `FileLike` con un valor por
defecto que reenvía a la versión síncrona, y luego sobrescribirlo por
dispositivo. No se hizo por un solo ioctl porque el riesgo es la amplitud —
`FileLike` lo implementan todos los tipos de fichero.

## Los otros hallazgos de la misma ejecución, cerrados

* Se sospechaba que `CREATE+DESTROY_DUMB` ponía a cero de forma síncrona. Lo
  hace, y es correcto (Linux también pone a cero los dumb buffers). La medida
  ya lo dice: 1,246 ms para un búfer de 800x600x4, frente a 1,875 MiB a la
  velocidad de escritura del framebuffer medida, 1471 MiB/s = 1,27 ms. Es la
  puesta a cero, y es el mismo 54x del suelo del ioctl que paga Linux (57x).
* Todas las demás filas de la sección cuestan lo mismo que en Linux o menos,
  en relación con el suelo de ioctl de esta misma máquina — así que
  `WAIT_VBLANK` era el único defecto real.
* Que `SET_CLIENT_CAP(ATOMIC)` devuelva `EOPNOTSUPP` no es un bug: la uAPI
  atómica es opt-in tras el flag `drm.atomic` de la cmdline, imitando a
  propósito a un driver de Linux sin `DRIVER_ATOMIC`.
