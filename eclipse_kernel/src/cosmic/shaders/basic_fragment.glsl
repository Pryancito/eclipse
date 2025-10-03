#version 450 core

in vec2 TexCoord;
in vec3 Normal;
in vec3 FragPos;

uniform vec3 uLightPos;
uniform vec3 uViewPos;
uniform vec3 uLightColor;
uniform vec3 uObjectColor;
uniform sampler2D uTexture;

out vec4 FragColor;

void main()
{
    // Ambient lighting
    float ambientStrength = 0.1;
    vec3 ambient = ambientStrength * uLightColor;
    
    // Diffuse lighting
    vec3 norm = normalize(Normal);
    vec3 lightDir = normalize(uLightPos - FragPos);
    float diff = max(dot(norm, lightDir), 0.0);
    vec3 diffuse = diff * uLightColor;
    
    // Specular lighting
    float specularStrength = 0.5;
    vec3 viewDir = normalize(uViewPos - FragPos);
    vec3 reflectDir = reflect(-lightDir, norm);
    float spec = pow(max(dot(viewDir, reflectDir), 0.0), 32);
    vec3 specular = specularStrength * spec * uLightColor;
    
    // Combine lighting
    vec3 result = (ambient + diffuse + specular) * uObjectColor;
    
    // Apply texture
    vec4 texColor = texture(uTexture, TexCoord);
    FragColor = vec4(result, 1.0) * texColor;
}
