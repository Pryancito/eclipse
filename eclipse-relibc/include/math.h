/* math.h for Eclipse OS */
#pragma once
#ifndef _MATH_H
#define _MATH_H

double fabs(double x);
double floor(double x);
double ceil(double x);
double sqrt(double x);
double pow(double x, double y);
double sin(double x);
double cos(double x);
double tan(double x);
double atan(double x);
double atan2(double y, double x);
double exp(double x);
double log(double x);
double log2(double x);
double log10(double x);
double fmod(double x, double y);
double modf(double x, double *iptr);

#define HUGE_VAL __builtin_huge_val()
#define INFINITY __builtin_inff()
#define NAN      __builtin_nanf("")
#define M_PI     3.14159265358979323846
#define M_E      2.71828182845904523536

#endif /* _MATH_H */
