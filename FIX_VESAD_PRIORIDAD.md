# 🔧 FIX: vesad tiene prioridad sobre nvidiad

## 🎯 Problema Detectado

En tus logs de QEMU viste:
```
vesad: 1280x800 stride 1280 at 0xC0000000
vesad done.
```

Esto significa que `vesad` está en el **initfs** y se carga MUY TEMPRANO, tomando el framebuffer antes de que `nvidiad` pueda hacerlo.

## ⚡ Solución: Agregar nvidiad a drivers-initfs

Los drivers que se cargan TEMPRANO están en `drivers-initfs`. Voy a agregar nvidiad ahí.

### Modificar drivers-initfs/recipe.toml

Agregar nvidiad, amdd, inteld a la lista de drivers del initfs para x86_64.


